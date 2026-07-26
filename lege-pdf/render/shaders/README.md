# shaders/

Shared renderer shaders may live here later. The initial decoded-image
compute shader is colocated with its sole owner at
`crates/pdf-render-wgpu/src/image.wgsl`; keeping `include_str!` local makes
crate packaging self-contained.
