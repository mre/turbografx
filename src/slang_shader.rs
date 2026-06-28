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

    pub fn render(
        &mut self,
        input: &Texture2D,
        source_width: usize,
        source_height: usize,
        frame_count: usize,
    ) -> Result<&Texture2D, String> {
        let output_size = (screen_width().ceil() as u32, screen_height().ceil() as u32);
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
            // librashader leaves raw GL state active. Macroquad will issue more
            // draw calls afterwards, so return to the default framebuffer.
            self.context.bind_framebuffer(glow::FRAMEBUFFER, None);
        }

        Ok(&self
            .output
            .as_ref()
            .expect("output target was created")
            .texture)
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
}

fn gl_image_from_texture(
    quad_context: &mut dyn miniquad::RenderingBackend,
    texture: miniquad::TextureId,
    width: u32,
    height: u32,
) -> Result<GLImage, String> {
    let raw = unsafe { quad_context.texture_raw_id(texture) };
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
