use bevy_app::Plugin;
use bevy_asset::{load_internal_asset, AssetServer, Handle, HandleUntyped};
use bevy_core_pipeline::{
    prelude::Camera3d,
    prepass::{AlphaMask3dPrepass, Opaque3dPrepass, PrepassSettings, ViewPrepassTextures},
};
use bevy_ecs::{
    prelude::Entity,
    query::With,
    system::{
        lifetimeless::{Read, SQuery, SRes},
        Commands, Query, Res, ResMut, Resource, SystemParamItem,
    },
    world::{FromWorld, World},
};
use bevy_reflect::TypeUuid;
use bevy_render::{
    camera::ExtractedCamera,
    mesh::MeshVertexBufferLayout,
    prelude::{Camera, Mesh},
    render_asset::RenderAssets,
    render_phase::{
        sort_phase_system, AddRenderCommand, DrawFunctions, EntityRenderCommand,
        RenderCommandResult, RenderPhase, SetItemPipeline, TrackedRenderPass,
    },
    render_resource::{
        BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
        BindGroupLayoutEntry, BindingType, BlendState, BufferBindingType, ColorTargetState,
        ColorWrites, CompareFunction, DepthBiasState, DepthStencilState, Extent3d, FragmentState,
        FrontFace, MultisampleState, PipelineCache, PolygonMode, PrimitiveState,
        RenderPipelineDescriptor, Shader, ShaderRef, ShaderStages, ShaderType,
        SpecializedMeshPipeline, SpecializedMeshPipelineError, SpecializedMeshPipelines,
        StencilFaceState, StencilState, TextureDescriptor, TextureDimension, TextureFormat,
        TextureUsages, VertexState,
    },
    renderer::RenderDevice,
    texture::TextureCache,
    view::{ExtractedView, Msaa, ViewUniform, ViewUniformOffset, ViewUniforms, VisibleEntities},
    Extract, RenderApp, RenderStage,
};
use bevy_utils::{tracing::error, HashMap};

use crate::{
    AlphaMode, DrawMesh, Material, MaterialPipeline, MaterialPipelineKey, MeshPipeline,
    MeshPipelineKey, MeshUniform, RenderMaterials, SetMaterialBindGroup, SetMeshBindGroup,
};

use std::{hash::Hash, marker::PhantomData};

pub const PREPASS_FORMAT: TextureFormat = TextureFormat::Depth32Float;

pub const PREPASS_SHADER_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 921124473254008983);

pub const PREPASS_BINDINGS_SHADER_HANDLE: HandleUntyped =
    HandleUntyped::weak_from_u64(Shader::TYPE_UUID, 5533152893177403494);

pub struct PrepassPlugin<M: Material>(PhantomData<M>);

impl<M: Material> Default for PrepassPlugin<M> {
    fn default() -> Self {
        Self(Default::default())
    }
}

impl<M: Material> Plugin for PrepassPlugin<M>
where
    M::Data: PartialEq + Eq + Hash + Clone,
{
    fn build(&self, app: &mut bevy_app::App) {
        load_internal_asset!(
            app,
            PREPASS_SHADER_HANDLE,
            "prepass.wgsl",
            Shader::from_wgsl
        );

        load_internal_asset!(
            app,
            PREPASS_BINDINGS_SHADER_HANDLE,
            "prepass_bindings.wgsl",
            Shader::from_wgsl
        );

        let render_app = match app.get_sub_app_mut(RenderApp) {
            Ok(render_app) => render_app,
            Err(_) => return,
        };

        render_app
            .add_system_to_stage(RenderStage::Extract, extract_core_3d_camera_prepass_phase)
            .add_system_to_stage(RenderStage::Prepare, prepare_core_3d_prepass_textures)
            .add_system_to_stage(RenderStage::Queue, queue_prepass_view_bind_group::<M>)
            .add_system_to_stage(RenderStage::Queue, queue_prepass_material_meshes::<M>)
            .add_system_to_stage(RenderStage::PhaseSort, sort_phase_system::<Opaque3dPrepass>)
            .add_system_to_stage(
                RenderStage::PhaseSort,
                sort_phase_system::<AlphaMask3dPrepass>,
            )
            .init_resource::<PrepassPipeline<M>>()
            .init_resource::<DrawFunctions<Opaque3dPrepass>>()
            .init_resource::<DrawFunctions<AlphaMask3dPrepass>>()
            .init_resource::<PrepassViewBindGroup>()
            .init_resource::<SpecializedMeshPipelines<PrepassPipeline<M>>>()
            .add_render_command::<Opaque3dPrepass, DrawPrepass<M>>()
            .add_render_command::<AlphaMask3dPrepass, DrawPrepass<M>>();
    }
}

#[derive(Resource)]
pub struct PrepassPipeline<M: Material> {
    pub view_layout: BindGroupLayout,
    pub mesh_layout: BindGroupLayout,
    pub skinned_mesh_layout: BindGroupLayout,
    pub material_layout: BindGroupLayout,
    pub material_vertex_shader: Option<Handle<Shader>>,
    pub material_fragment_shader: Option<Handle<Shader>>,
    pub material_pipeline: MaterialPipeline<M>,
    _marker: PhantomData<M>,
}

impl<M: Material> FromWorld for PrepassPipeline<M> {
    fn from_world(world: &mut World) -> Self {
        let render_device = world.resource::<RenderDevice>();
        let asset_server = world.resource::<AssetServer>();

        let view_layout = render_device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            entries: &[
                // View
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: Some(ViewUniform::min_size()),
                    },
                    count: None,
                },
            ],
            label: Some("prepass_view_layout"),
        });

        let mesh_pipeline = world.resource::<MeshPipeline>();

        PrepassPipeline {
            view_layout,
            mesh_layout: mesh_pipeline.mesh_layout.clone(),
            skinned_mesh_layout: mesh_pipeline.skinned_mesh_layout.clone(),
            material_vertex_shader: match M::prepass_vertex_shader() {
                ShaderRef::Default => None,
                ShaderRef::Handle(handle) => Some(handle),
                ShaderRef::Path(path) => Some(asset_server.load(path)),
            },
            material_fragment_shader: match M::prepass_fragment_shader() {
                ShaderRef::Default => None,
                ShaderRef::Handle(handle) => Some(handle),
                ShaderRef::Path(path) => Some(asset_server.load(path)),
            },
            material_layout: M::bind_group_layout(render_device),
            material_pipeline: world.resource::<MaterialPipeline<M>>().clone(),
            _marker: PhantomData,
        }
    }
}

impl<M: Material> SpecializedMeshPipeline for PrepassPipeline<M>
where
    M::Data: PartialEq + Eq + Hash + Clone,
{
    type Key = MaterialPipelineKey<M>;

    fn specialize(
        &self,
        key: Self::Key,
        layout: &MeshVertexBufferLayout,
    ) -> Result<RenderPipelineDescriptor, SpecializedMeshPipelineError> {
        let mut bind_group_layout = vec![self.view_layout.clone()];
        let mut shader_defs = Vec::new();
        let mut vertex_attributes = Vec::new();

        // NOTE: Eventually, it would be nice to only add this when the shaders area overloaded.
        // The main limitation right now is that bind group order is hardcoded in shaders.
        bind_group_layout.insert(1, self.material_layout.clone());

        if key.mesh_key.contains(MeshPipelineKey::PREPASS_DEPTH) {
            shader_defs.push(String::from("PREPASS_DEPTH"));
        }

        if key.mesh_key.contains(MeshPipelineKey::ALPHA_MASK) {
            shader_defs.push(String::from("ALPHA_MASK"));
        }

        if layout.contains(Mesh::ATTRIBUTE_POSITION) {
            shader_defs.push(String::from("VERTEX_POSITIONS"));
            vertex_attributes.push(Mesh::ATTRIBUTE_POSITION.at_shader_location(0));
        }

        if layout.contains(Mesh::ATTRIBUTE_UV_0) {
            shader_defs.push(String::from("VERTEX_UVS"));
            vertex_attributes.push(Mesh::ATTRIBUTE_UV_0.at_shader_location(1));
        }

        if key.mesh_key.contains(MeshPipelineKey::PREPASS_NORMALS) {
            vertex_attributes.push(Mesh::ATTRIBUTE_NORMAL.at_shader_location(2));
            shader_defs.push(String::from("PREPASS_NORMALS"));

            if layout.contains(Mesh::ATTRIBUTE_TANGENT) {
                shader_defs.push(String::from("VERTEX_TANGENTS"));
                vertex_attributes.push(Mesh::ATTRIBUTE_TANGENT.at_shader_location(3));
            }
        }

        if layout.contains(Mesh::ATTRIBUTE_JOINT_INDEX)
            && layout.contains(Mesh::ATTRIBUTE_JOINT_WEIGHT)
        {
            shader_defs.push(String::from("SKINNED"));
            vertex_attributes.push(Mesh::ATTRIBUTE_JOINT_INDEX.at_shader_location(4));
            vertex_attributes.push(Mesh::ATTRIBUTE_JOINT_WEIGHT.at_shader_location(5));
            bind_group_layout.insert(2, self.skinned_mesh_layout.clone());
        } else {
            bind_group_layout.insert(2, self.mesh_layout.clone());
        }

        let vertex_buffer_layout = layout.get_layout(&vertex_attributes)?;

        let fragment = if key.mesh_key.contains(MeshPipelineKey::PREPASS_NORMALS)
            || key.mesh_key.contains(MeshPipelineKey::ALPHA_MASK)
        {
            let frag_shader_handle = if let Some(handle) = &self.material_fragment_shader {
                handle.clone()
            } else {
                PREPASS_SHADER_HANDLE.typed::<Shader>()
            };

            let mut targets = vec![];
            if key.mesh_key.contains(MeshPipelineKey::PREPASS_NORMALS) {
                targets.push(Some(ColorTargetState {
                    format: TextureFormat::Rgb10a2Unorm,
                    blend: Some(BlendState::REPLACE),
                    write_mask: ColorWrites::ALL,
                }));
            }

            Some(FragmentState {
                shader: frag_shader_handle,
                entry_point: "fragment".into(),
                shader_defs: shader_defs.clone(),
                targets,
            })
        } else {
            None
        };

        let vert_shader_handle = if let Some(handle) = &self.material_vertex_shader {
            handle.clone()
        } else {
            PREPASS_SHADER_HANDLE.typed::<Shader>()
        };

        let mut descriptor = RenderPipelineDescriptor {
            vertex: VertexState {
                shader: vert_shader_handle,
                entry_point: "vertex".into(),
                shader_defs,
                buffers: vec![vertex_buffer_layout],
            },
            fragment,
            layout: Some(bind_group_layout),
            primitive: PrimitiveState {
                topology: key.mesh_key.primitive_topology(),
                strip_index_format: None,
                front_face: FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(DepthStencilState {
                format: PREPASS_FORMAT,
                depth_write_enabled: true,
                depth_compare: CompareFunction::GreaterEqual,
                stencil: StencilState {
                    front: StencilFaceState::IGNORE,
                    back: StencilFaceState::IGNORE,
                    read_mask: 0,
                    write_mask: 0,
                },
                bias: DepthBiasState {
                    constant: 0,
                    slope_scale: 0.0,
                    clamp: 0.0,
                },
            }),
            multisample: MultisampleState {
                count: key.mesh_key.msaa_samples(),
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            label: Some("prepass_pipeline".into()),
        };

        // This is a bit risky because it's possible to change something that would
        // break the prepass but be fine in the main pass.
        // Since this api is pretty low-level it doesn't matter that much, but it is a potential issue.
        M::specialize(&self.material_pipeline, &mut descriptor, layout, key)?;

        Ok(descriptor)
    }
}

pub fn extract_core_3d_camera_prepass_phase(
    mut commands: Commands,
    cameras_3d: Extract<Query<(Entity, &Camera, &PrepassSettings), With<Camera3d>>>,
) {
    for (entity, camera, prepass_settings) in cameras_3d.iter() {
        if camera.is_active {
            commands.get_or_spawn(entity).insert((
                RenderPhase::<Opaque3dPrepass>::default(),
                RenderPhase::<AlphaMask3dPrepass>::default(),
                prepass_settings.clone(),
            ));
        }
    }
}

pub fn prepare_core_3d_prepass_textures(
    mut commands: Commands,
    mut texture_cache: ResMut<TextureCache>,
    msaa: Res<Msaa>,
    render_device: Res<RenderDevice>,
    views_3d: Query<
        (Entity, &ExtractedCamera, &PrepassSettings),
        (
            With<RenderPhase<Opaque3dPrepass>>,
            With<RenderPhase<AlphaMask3dPrepass>>,
        ),
    >,
) {
    let mut depth_textures = HashMap::default();
    let mut normal_textures = HashMap::default();
    for (entity, camera, prepass_settings) in &views_3d {
        if let Some(physical_target_size) = camera.physical_target_size {
            let size = Extent3d {
                depth_or_array_layers: 1,
                width: physical_target_size.x,
                height: physical_target_size.y,
            };

            let cached_depth_texture = prepass_settings.output_depth.then(|| {
                depth_textures
                    .entry(camera.target.clone())
                    .or_insert_with(|| {
                        texture_cache.get(
                            &render_device,
                            TextureDescriptor {
                                label: Some("view_depth_texture_resource"),
                                size,
                                mip_level_count: 1,
                                sample_count: msaa.samples,
                                dimension: TextureDimension::D2,
                                format: TextureFormat::Depth32Float,
                                usage: TextureUsages::COPY_DST
                                    | TextureUsages::RENDER_ATTACHMENT
                                    | TextureUsages::TEXTURE_BINDING,
                            },
                        )
                    })
                    .clone()
            });
            let cached_normals_texture = prepass_settings.output_normals.then(|| {
                normal_textures
                    .entry(camera.target.clone())
                    .or_insert_with(|| {
                        texture_cache.get(
                            &render_device,
                            TextureDescriptor {
                                label: Some("view_normals_texture"),
                                size,
                                mip_level_count: 1,
                                sample_count: msaa.samples,
                                dimension: TextureDimension::D2,
                                format: TextureFormat::Rgb10a2Unorm,
                                usage: TextureUsages::RENDER_ATTACHMENT
                                    | TextureUsages::TEXTURE_BINDING,
                            },
                        )
                    })
                    .clone()
            });
            commands.entity(entity).insert(ViewPrepassTextures {
                depth: cached_depth_texture,
                normals: cached_normals_texture,
                size,
            });
        }
    }
}

#[derive(Default, Resource)]
pub struct PrepassViewBindGroup {
    bind_group: Option<BindGroup>,
}

pub fn queue_prepass_view_bind_group<M: Material>(
    render_device: Res<RenderDevice>,
    prepass_pipeline: Res<PrepassPipeline<M>>,
    view_uniforms: Res<ViewUniforms>,
    mut prepass_view_bind_group: ResMut<PrepassViewBindGroup>,
) {
    if let Some(view_binding) = view_uniforms.uniforms.binding() {
        prepass_view_bind_group.bind_group =
            Some(render_device.create_bind_group(&BindGroupDescriptor {
                entries: &[BindGroupEntry {
                    binding: 0,
                    resource: view_binding,
                }],
                label: Some("prepass_view_bind_group"),
                layout: &prepass_pipeline.view_layout,
            }));
    }
}

#[allow(clippy::too_many_arguments)]
pub fn queue_prepass_material_meshes<M: Material>(
    opaque_draw_functions: Res<DrawFunctions<Opaque3dPrepass>>,
    alpha_mask_draw_functions: Res<DrawFunctions<AlphaMask3dPrepass>>,
    prepass_pipeline: Res<PrepassPipeline<M>>,
    mut pipelines: ResMut<SpecializedMeshPipelines<PrepassPipeline<M>>>,
    mut pipeline_cache: ResMut<PipelineCache>,
    msaa: Res<Msaa>,
    render_meshes: Res<RenderAssets<Mesh>>,
    render_materials: Res<RenderMaterials<M>>,
    material_meshes: Query<(&Handle<M>, &Handle<Mesh>, &MeshUniform)>,
    mut views: Query<(
        &ExtractedView,
        &VisibleEntities,
        &PrepassSettings,
        &mut RenderPhase<Opaque3dPrepass>,
        &mut RenderPhase<AlphaMask3dPrepass>,
    )>,
) where
    M::Data: PartialEq + Eq + Hash + Clone,
{
    let opaque_draw_prepass = opaque_draw_functions
        .read()
        .get_id::<DrawPrepass<M>>()
        .unwrap();
    let alpha_mask_draw_prepass = alpha_mask_draw_functions
        .read()
        .get_id::<DrawPrepass<M>>()
        .unwrap();
    for (view, visible_entities, prepass_settings, mut opaque_phase, mut alpha_mask_phase) in
        &mut views
    {
        let rangefinder = view.rangefinder3d();

        let mut view_key =
            MeshPipelineKey::PREPASS_DEPTH | MeshPipelineKey::from_msaa_samples(msaa.samples);
        if prepass_settings.output_normals {
            view_key |= MeshPipelineKey::PREPASS_NORMALS;
        }

        for visible_entity in &visible_entities.entities {
            if let Ok((material_handle, mesh_handle, mesh_uniform)) =
                material_meshes.get(*visible_entity)
            {
                if let (Some(material), Some(mesh)) = (
                    render_materials.get(material_handle),
                    render_meshes.get(mesh_handle),
                ) {
                    let mut key = MeshPipelineKey::from_primitive_topology(mesh.primitive_topology)
                        | view_key;
                    let alpha_mode = material.properties.alpha_mode;
                    match alpha_mode {
                        AlphaMode::Opaque => {}
                        AlphaMode::Mask(_) => key |= MeshPipelineKey::ALPHA_MASK,
                        AlphaMode::Blend => continue,
                    }

                    let pipeline_id = pipelines.specialize(
                        &mut pipeline_cache,
                        &prepass_pipeline,
                        MaterialPipelineKey {
                            mesh_key: key,
                            bind_group_data: material.key.clone(),
                        },
                        &mesh.layout,
                    );
                    let pipeline_id = match pipeline_id {
                        Ok(id) => id,
                        Err(err) => {
                            error!("{}", err);
                            continue;
                        }
                    };

                    let distance = rangefinder.distance(&mesh_uniform.transform)
                        + material.properties.depth_bias;
                    match alpha_mode {
                        AlphaMode::Opaque => {
                            opaque_phase.add(Opaque3dPrepass {
                                entity: *visible_entity,
                                draw_function: opaque_draw_prepass,
                                pipeline_id,
                                distance,
                            });
                        }
                        AlphaMode::Mask(_) => {
                            alpha_mask_phase.add(AlphaMask3dPrepass {
                                entity: *visible_entity,
                                draw_function: alpha_mask_draw_prepass,
                                pipeline_id,
                                distance,
                            });
                        }
                        AlphaMode::Blend => {}
                    }
                }
            }
        }
    }
}

pub struct SetPrepassViewBindGroup<const I: usize>;
impl<const I: usize> EntityRenderCommand for SetPrepassViewBindGroup<I> {
    type Param = (SRes<PrepassViewBindGroup>, SQuery<Read<ViewUniformOffset>>);
    #[inline]
    fn render<'w>(
        view: Entity,
        _item: Entity,
        (prepass_view_bind_group, view_query): SystemParamItem<'w, '_, Self::Param>,
        pass: &mut TrackedRenderPass<'w>,
    ) -> RenderCommandResult {
        let view_uniform_offset = view_query.get(view).unwrap();
        let prepass_view_bind_group = prepass_view_bind_group.into_inner();
        pass.set_bind_group(
            I,
            prepass_view_bind_group.bind_group.as_ref().unwrap(),
            &[view_uniform_offset.offset],
        );

        RenderCommandResult::Success
    }
}

pub type DrawPrepass<M> = (
    SetItemPipeline,
    SetPrepassViewBindGroup<0>,
    SetMaterialBindGroup<M, 1>,
    SetMeshBindGroup<2>,
    DrawMesh,
);
