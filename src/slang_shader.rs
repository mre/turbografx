use glow::HasContext;
use librashader::presets::ShaderFeatures;
use librashader::runtime::gl::{FilterChain, FilterChainOptions, FrameOptions, GLImage};
use librashader::runtime::{Size, Viewport};
use macroquad::prelude::*;
use macroquad::window::miniquad::{self, RawId};
use std::ffi::{CString, c_void};
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;

pub struct SlangShader {
    chain: FilterChain,
    context: Arc<glow::Context>,
    output: Option<RenderTarget>,
    output_size: (u32, u32),
    // Keep the loaded OpenGL library alive for the function pointers held by glow.
    _loader: GlProcLoader,
}

impl SlangShader {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let loader = GlProcLoader::new()?;
        let context = Arc::new(unsafe {
            glow::Context::from_loader_function(|name| loader.get_proc_address(name))
        });
        let options = FilterChainOptions {
            // Keep the compatibility path so this can run on OpenGL 3.3 contexts.
            glsl_version: 330,
            use_dsa: false,
            force_no_mipmaps: false,
            disable_cache: false,
        };
        let chain = unsafe {
            FilterChain::load_from_path(
                path,
                ShaderFeatures::NONE,
                Arc::clone(&context),
                Some(&options),
            )
        }
        .map_err(|err| format!("failed to load shader preset: {err}"))?;

        Ok(Self {
            chain,
            context,
            output: None,
            output_size: (0, 0),
            _loader: loader,
        })
    }

    /// Run the shader chain on `input` and blit the result straight to the
    /// default framebuffer (the window). We deliberately do not hand the result
    /// back to macroquad to draw: librashader leaves raw GL state bound (sampler
    /// objects on the texture units, its own program and VAO) and macroquad's
    /// render-target draw path samples as black on macOS once that state is
    /// active. A direct framebuffer blit is immune to both problems.
    pub fn render(
        &mut self,
        input: &Texture2D,
        source_width: usize,
        source_height: usize,
        frame_count: usize,
        dest: (f32, f32, f32, f32),
    ) -> Result<(), String> {
        let (dest_x, dest_y, dest_w, dest_h) = dest;
        // The shader chain renders at the size of the (letterboxed) destination
        // rectangle, so CRT geometry is computed for the visible 4:3 area.
        let output_size = (
            (dest_w.round() as u32).max(1),
            (dest_h.round() as u32).max(1),
        );
        self.ensure_output(output_size)?;

        let input_id = input.raw_miniquad_id();
        let output = self.output.as_ref().expect("output target was created");
        let output_id = output.texture.raw_miniquad_id();

        let mut internal_gl = unsafe { macroquad::window::get_internal_gl() };
        internal_gl.flush();

        let input = gl_image_from_texture(
            internal_gl.quad_context,
            input_id,
            source_width as u32,
            source_height as u32,
        )?;
        let output_image = gl_image_from_texture(
            internal_gl.quad_context,
            output_id,
            output_size.0,
            output_size.1,
        )?;
        // librashader always selects a *_MIPMAP_* minification filter for the
        // source texture (see `gl_mip`), but the frame texture macroquad hands us
        // only has mip level 0. macOS's OpenGL driver flags such a texture as
        // mipmap-incomplete and samples zero/black instead. Clamp the mip range
        // to level 0 so the texture is complete for a mipmap filter.
        self.clamp_to_base_level(input.handle);
        let viewport = Viewport {
            x: 0.0,
            y: 0.0,
            mvp: None,
            output: &output_image,
            size: Size {
                width: output_size.0,
                height: output_size.1,
            },
        };

        unsafe {
            self.chain
                .frame(
                    &input,
                    &viewport,
                    frame_count,
                    Some(&FrameOptions::default()),
                )
                .map_err(|err| format!("failed to render shader frame: {err}"))?;

            // Blit librashader's output texture into the default framebuffer (the
            // window). The source FBO wraps the output texture for reading; the
            // window is FBO 0. The output is placed in the centred 4:3
            // destination rectangle; the rest of the window is cleared to black
            // for the letterbox bars. The destination Y range is flipped because
            // the chain renders bottom-up while the window expects top-down.
            let screen_h = screen_height();
            let dst_x0 = dest_x.round() as i32;
            let dst_x1 = (dest_x + dest_w).round() as i32;
            let dst_top = (screen_h - dest_y).round() as i32;
            let dst_bottom = (screen_h - dest_y - dest_h).round() as i32;

            self.context.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.context.disable(glow::SCISSOR_TEST);
            self.context.clear_color(0.0, 0.0, 0.0, 1.0);
            self.context.clear(glow::COLOR_BUFFER_BIT);

            let read_fbo = self
                .context
                .create_framebuffer()
                .map_err(|err| format!("failed to create blit framebuffer: {err}"))?;
            self.context
                .bind_framebuffer(glow::READ_FRAMEBUFFER, Some(read_fbo));
            self.context.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                output_image.handle,
                0,
            );
            self.context.bind_framebuffer(glow::DRAW_FRAMEBUFFER, None);
            self.context.blit_framebuffer(
                0,
                0,
                output_size.0 as i32,
                output_size.1 as i32,
                dst_x0,
                dst_top,
                dst_x1,
                dst_bottom,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
            self.context.bind_framebuffer(glow::READ_FRAMEBUFFER, None);
            self.context.delete_framebuffer(read_fbo);
            self.context.bind_framebuffer(glow::FRAMEBUFFER, None);
        }

        Ok(())
    }

    fn ensure_output(&mut self, size: (u32, u32)) -> Result<(), String> {
        let size = (size.0.max(1), size.1.max(1));
        if self.output.is_some() && self.output_size == size {
            return Ok(());
        }

        let target = render_target(size.0, size.1);
        target.texture.set_filter(FilterMode::Nearest);
        self.output = Some(target);
        self.output_size = size;
        Ok(())
    }

    /// Restrict a texture to mip level 0 so it is complete when sampled with a
    /// mipmap minification filter. macroquad-created textures only have level 0,
    /// but both librashader and macroquad sample with `*_MIPMAP_*` filters;
    /// macOS's OpenGL driver otherwise rejects them as incomplete and samples
    /// black.
    fn clamp_to_base_level(&self, handle: Option<glow::NativeTexture>) {
        let Some(handle) = handle else { return };
        unsafe {
            self.context.bind_texture(glow::TEXTURE_2D, Some(handle));
            self.context
                .tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_BASE_LEVEL, 0);
            self.context
                .tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAX_LEVEL, 0);
            self.context.bind_texture(glow::TEXTURE_2D, None);
        }
    }
}

fn gl_image_from_texture(
    quad_context: &mut dyn miniquad::RenderingBackend,
    texture: miniquad::TextureId,
    width: u32,
    height: u32,
) -> Result<GLImage, String> {
    let raw = unsafe { quad_context.texture_raw_id(texture) };
    // `RawId` only has the `OpenGl` variant on targets without a Metal backend
    // (e.g. Linux/Windows CI), which makes this pattern irrefutable there.
    #[allow(irrefutable_let_patterns)]
    let RawId::OpenGl(raw) = raw else {
        return Err("slang shaders require macroquad's OpenGL backend".to_owned());
    };
    let Some(raw) = NonZeroU32::new(raw) else {
        return Err("OpenGL texture handle was zero".to_owned());
    };

    Ok(GLImage {
        handle: Some(glow::NativeTexture(raw)),
        format: glow::RGBA8,
        size: Size { width, height },
    })
}

struct GlProcLoader {
    libraries: Vec<libloading::Library>,
}

impl GlProcLoader {
    fn new() -> Result<Self, String> {
        let mut libraries = Vec::new();
        for name in gl_library_names() {
            if let Ok(library) = unsafe { libloading::Library::new(name) } {
                libraries.push(library);
            }
        }
        if libraries.is_empty() {
            return Err("failed to load an OpenGL library for librashader".to_owned());
        }
        Ok(Self { libraries })
    }

    fn get_proc_address(&self, name: &str) -> *const c_void {
        let Ok(name) = CString::new(name) else {
            return std::ptr::null();
        };

        for library in &self.libraries {
            if let Ok(symbol) = unsafe { library.get::<*const c_void>(name.as_bytes_with_nul()) } {
                return *symbol;
            }
        }

        for loader_name in [
            "glXGetProcAddressARB",
            "glXGetProcAddress",
            "wglGetProcAddress",
            "eglGetProcAddress",
        ] {
            for library in &self.libraries {
                if let Ok(loader) = unsafe {
                    library.get::<unsafe extern "C" fn(*const u8) -> *const c_void>(
                        CString::new(loader_name)
                            .expect("static name")
                            .as_bytes_with_nul(),
                    )
                } {
                    let ptr = unsafe { loader(name.as_ptr().cast()) };
                    if !ptr.is_null() {
                        return ptr;
                    }
                }
            }
        }

        std::ptr::null()
    }
}

#[cfg(target_os = "windows")]
fn gl_library_names() -> &'static [&'static str] {
    &["opengl32.dll"]
}

#[cfg(target_os = "macos")]
fn gl_library_names() -> &'static [&'static str] {
    &["/System/Library/Frameworks/OpenGL.framework/OpenGL"]
}

#[cfg(all(unix, not(target_os = "macos")))]
fn gl_library_names() -> &'static [&'static str] {
    &["libGL.so.1", "libGL.so", "libEGL.so.1", "libEGL.so"]
}

#[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
fn gl_library_names() -> &'static [&'static str] {
    &[]
}
