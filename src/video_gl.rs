//! Draws planar YUV frames with a small shader, so colour conversion and scaling happen on the
//! GPU and a 1080p frame costs a 3 MB texture upload instead of an 8 MB RGBA one.

use eframe::glow::{self, HasContext};

use crate::media::VideoFrame;

const VERTEX: &str = r#"
in vec2 a_pos;
out vec2 v_uv;
void main() {
    v_uv = vec2(a_pos.x * 0.5 + 0.5, 0.5 - a_pos.y * 0.5);
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
"#;

// Limited-range YCbCr to RGB. `u_matrix` selects BT.709 (HD) or BT.601 (SD) coefficients.
const FRAGMENT: &str = r#"
in vec2 v_uv;
out vec4 o_color;
uniform sampler2D u_y;
uniform sampler2D u_u;
uniform sampler2D u_v;
uniform vec4 u_matrix;
void main() {
    float y = (texture(u_y, v_uv).r - 0.0627451) * 1.1643836;
    float cb = texture(u_u, v_uv).r - 0.5019608;
    float cr = texture(u_v, v_uv).r - 0.5019608;
    vec3 rgb = vec3(y + u_matrix.x * cr, y - u_matrix.y * cb - u_matrix.z * cr, y + u_matrix.w * cb);
    o_color = vec4(clamp(rgb, 0.0, 1.0), 1.0);
}
"#;

struct Resources {
    program: glow::Program,
    vao: glow::VertexArray,
    _vbo: glow::Buffer,
    textures: [glow::Texture; 3],
    /// Allocated size of each plane texture, to tell a re-upload from a reallocation.
    sizes: [(u32, u32); 3],
    matrix: [f32; 4],
}

#[derive(Default)]
pub struct VideoRenderer {
    pending: Option<VideoFrame>,
    res: Option<Resources>,
    has_picture: bool,
    error: Option<String>,
}

impl VideoRenderer {
    /// Hands over the next picture; it is uploaded on the following paint.
    pub fn set_frame(&mut self, frame: VideoFrame) {
        self.pending = Some(frame);
    }

    pub fn clear(&mut self) {
        self.pending = None;
        self.has_picture = false;
    }

    /// Why nothing is being drawn, if the GPU rejected the shader or its buffers.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Must run with the GL context current (inside egui's paint callback). The viewport is
    /// already set to the rectangle the picture should fill.
    pub fn paint(&mut self, gl: &glow::Context) {
        if self.error.is_some() {
            return;
        }
        if self.res.is_none() {
            match unsafe { create_resources(gl) } {
                Ok(r) => self.res = Some(r),
                Err(e) => {
                    self.error = Some(format!("video renderer: {}", e.trim()));
                    return;
                }
            }
        }
        let Some(res) = self.res.as_mut() else { return };
        unsafe {
            if let Some(frame) = self.pending.take() {
                upload(gl, res, &frame);
                self.has_picture = true;
            }
            if !self.has_picture {
                return;
            }
            gl.disable(glow::BLEND);
            gl.use_program(Some(res.program));
            for (unit, (tex, name)) in res.textures.iter().zip(["u_y", "u_u", "u_v"]).enumerate() {
                gl.active_texture(glow::TEXTURE0 + unit as u32);
                gl.bind_texture(glow::TEXTURE_2D, Some(*tex));
                gl.uniform_1_i32(gl.get_uniform_location(res.program, name).as_ref(), unit as i32);
            }
            let m = res.matrix;
            gl.uniform_4_f32(gl.get_uniform_location(res.program, "u_matrix").as_ref(), m[0], m[1], m[2], m[3]);
            gl.bind_vertex_array(Some(res.vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            gl.bind_vertex_array(None);
            gl.active_texture(glow::TEXTURE0);
            gl.enable(glow::BLEND);
        }
    }
}

unsafe fn create_resources(gl: &glow::Context) -> Result<Resources, String> {
    unsafe {
        let header = if gl.version().is_embedded { "#version 300 es\nprecision mediump float;\n" } else { "#version 140\n" };
        let program = gl.create_program()?;
        let mut shaders = Vec::new();
        for (kind, src) in [(glow::VERTEX_SHADER, VERTEX), (glow::FRAGMENT_SHADER, FRAGMENT)] {
            let shader = gl.create_shader(kind)?;
            gl.shader_source(shader, &format!("{header}{src}"));
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                return Err(gl.get_shader_info_log(shader));
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        gl.bind_attrib_location(program, 0, "a_pos");
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            return Err(gl.get_program_info_log(program));
        }
        for shader in shaders {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }

        let vao = gl.create_vertex_array()?;
        let vbo = gl.create_buffer()?;
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        let quad: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
        let bytes: Vec<u8> = quad.iter().flat_map(|f| f.to_ne_bytes()).collect();
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STATIC_DRAW);
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
        gl.bind_vertex_array(None);
        gl.bind_buffer(glow::ARRAY_BUFFER, None);

        let mut textures = Vec::new();
        for _ in 0..3 {
            let tex = gl.create_texture()?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            for (param, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, param, value as i32);
            }
            textures.push(tex);
        }
        gl.bind_texture(glow::TEXTURE_2D, None);
        Ok(Resources {
            program,
            vao,
            _vbo: vbo,
            textures: [textures[0], textures[1], textures[2]],
            sizes: [(0, 0); 3],
            matrix: [0.0; 4],
        })
    }
}

unsafe fn upload(gl: &glow::Context, res: &mut Resources, f: &VideoFrame) {
    let planes: [(&[u8], u32, u32, u32); 3] = [
        (&f.y, f.width, f.height, f.y_stride),
        (&f.u, f.chroma_width, f.chroma_height, f.c_stride),
        (&f.v, f.chroma_width, f.chroma_height, f.c_stride),
    ];
    unsafe {
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        for (i, (data, w, h, stride)) in planes.into_iter().enumerate() {
            // A short plane would make the driver read out of bounds; skip the frame instead.
            let needed = if h == 0 { 0 } else { stride as usize * (h as usize - 1) + w as usize };
            if w == 0 || h == 0 || stride < w || data.len() < needed {
                continue;
            }
            gl.bind_texture(glow::TEXTURE_2D, Some(res.textures[i]));
            // Padded planes: upload only the visible columns of each row.
            gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, stride as i32);
            let pixels = glow::PixelUnpackData::Slice(Some(data));
            if res.sizes[i] == (w, h) {
                gl.tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, w as i32, h as i32, glow::RED, glow::UNSIGNED_BYTE, pixels);
            } else {
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::R8 as i32,
                    w as i32,
                    h as i32,
                    0,
                    glow::RED,
                    glow::UNSIGNED_BYTE,
                    pixels,
                );
                res.sizes[i] = (w, h);
            }
        }
        gl.pixel_store_i32(glow::UNPACK_ROW_LENGTH, 0);
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
        gl.bind_texture(glow::TEXTURE_2D, None);
    }
    // HD material is BT.709, SD is BT.601 — the usual convention when a stream does not say.
    res.matrix = if f.height >= 720 || f.width >= 1280 {
        [1.7927411, 0.2132486, 0.5329093, 2.1124018]
    } else {
        [1.5960268, 0.3917623, 0.8129676, 2.0172321]
    };
}
