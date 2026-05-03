use std::collections::HashMap;
use std::error::Error;

use glow::HasContext;

pub type Color = [f32; 4];

pub const BG: Color = [0.06, 0.06, 0.08, 0.94];
pub const FG: Color = [0.92, 0.92, 0.94, 1.0];
pub const FG_DIM: Color = [0.55, 0.55, 0.58, 1.0];
pub const ACCENT: Color = [0.27, 0.55, 0.95, 1.0];
pub const SEL_BG: Color = [0.20, 0.22, 0.30, 1.0];

const ATLAS_W: u32 = 1024;
const ATLAS_H: u32 = 1024;

const VS_SRC: &str = "#version 330 core\n\
layout(location=0) in vec2 a_pos;\n\
layout(location=1) in vec2 a_uv;\n\
layout(location=2) in vec4 a_color;\n\
uniform vec2 u_screen;\n\
out vec2 v_uv;\n\
out vec4 v_color;\n\
void main() {\n\
    vec2 p = (a_pos / u_screen) * 2.0 - 1.0;\n\
    p.y = -p.y;\n\
    gl_Position = vec4(p, 0.0, 1.0);\n\
    v_uv = a_uv;\n\
    v_color = a_color;\n\
}";

// All atlas content is stored premultiplied: glyphs as (c,c,c,c) where c is
// coverage, icons as actual premultiplied RGBA, white-pixel as (1,1,1,1).
// Vertex `a_color` is also premultiplied (push_quad multiplies on the CPU
// side), so the shader is just a plain modulate.
const FS_SRC: &str = "#version 330 core\n\
in vec2 v_uv;\n\
in vec4 v_color;\n\
uniform sampler2D u_atlas;\n\
out vec4 frag_color;\n\
void main() {\n\
    frag_color = v_color * texture(u_atlas, v_uv);\n\
}";

#[repr(C)]
#[derive(Copy, Clone)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
}

#[derive(Copy, Clone, Debug)]
struct GlyphMetrics {
    uv: [f32; 4],
    size: [f32; 2],
    bearing_x: f32,
    bearing_ymin: i32,
    advance: f32,
}

impl GlyphMetrics {
    fn empty(advance: f32) -> Self {
        Self {
            uv: [0.0; 4],
            size: [0.0; 2],
            bearing_x: 0.0,
            bearing_ymin: 0,
            advance,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct IconHandle {
    pub uv: [f32; 4],
    #[allow(dead_code)] // exposed for callers that want native-resolution layout
    pub size: [f32; 2],
}

pub struct Renderer {
    program: glow::Program,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    u_screen: glow::UniformLocation,
    atlas: Atlas,
    vertices: Vec<Vertex>,
    screen_w: f32,
    screen_h: f32,
}

struct Atlas {
    texture: glow::Texture,
    cursor_x: u32,
    cursor_y: u32,
    row_height: u32,
    white_uv: [f32; 4],
    glyphs: HashMap<(u32, u32), GlyphMetrics>,
    icons: HashMap<String, IconHandle>,
    font: fontdue::Font,
}

impl Renderer {
    pub fn new(gl: &glow::Context, font_data: &[u8]) -> Result<Self, Box<dyn Error>> {
        let font = fontdue::Font::from_bytes(font_data, fontdue::FontSettings::default())
            .map_err(|e| format!("fontdue: {e}"))?;
        unsafe {
            let program = compile_program(gl, VS_SRC, FS_SRC)?;
            let u_screen = gl
                .get_uniform_location(program, "u_screen")
                .ok_or("missing uniform u_screen")?;
            let u_atlas = gl
                .get_uniform_location(program, "u_atlas")
                .ok_or("missing uniform u_atlas")?;

            let vao = gl.create_vertex_array().map_err(|e| format!("vao: {e}"))?;
            let vbo = gl.create_buffer().map_err(|e| format!("vbo: {e}"))?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));

            let stride = std::mem::size_of::<Vertex>() as i32;
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 8);
            gl.enable_vertex_attrib_array(2);
            gl.vertex_attrib_pointer_f32(2, 4, glow::FLOAT, false, stride, 16);

            gl.bind_vertex_array(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);

            let atlas = Atlas::new(gl, font)?;

            gl.use_program(Some(program));
            gl.uniform_1_i32(Some(&u_atlas), 0);
            gl.use_program(None);

            Ok(Self {
                program,
                vao,
                vbo,
                u_screen,
                atlas,
                vertices: Vec::with_capacity(4096),
                screen_w: 0.0,
                screen_h: 0.0,
            })
        }
    }

    pub fn begin(&mut self, gl: &glow::Context, w: u32, h: u32) {
        self.screen_w = w as f32;
        self.screen_h = h as f32;
        self.vertices.clear();
        unsafe {
            gl.viewport(0, 0, w as i32, h as i32);
            gl.disable(glow::DEPTH_TEST);
            gl.enable(glow::BLEND);
            gl.blend_func_separate(
                glow::ONE,
                glow::ONE_MINUS_SRC_ALPHA,
                glow::ONE,
                glow::ONE_MINUS_SRC_ALPHA,
            );
            gl.clear_color(0.0, 0.0, 0.0, 0.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
    }

    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        let [u0, v0, u1, v1] = self.atlas.white_uv;
        self.push_quad(x, y, w, h, u0, v0, u1, v1, color);
    }

    pub fn draw_text(
        &mut self,
        gl: &glow::Context,
        x: f32,
        baseline_y: f32,
        size: f32,
        text: &str,
        color: Color,
    ) -> f32 {
        let mut pen_x = x;
        let size_int = size as u32;
        for ch in text.chars() {
            let key = (ch as u32, size_int);
            let g = if let Some(g) = self.atlas.glyphs.get(&key) {
                *g
            } else {
                let g = self.atlas.upload_glyph(gl, ch, size);
                self.atlas.glyphs.insert(key, g);
                g
            };
            if g.size[0] > 0.0 && g.size[1] > 0.0 {
                let qx = pen_x + g.bearing_x;
                let qy = baseline_y - g.size[1] - g.bearing_ymin as f32;
                self.push_quad(
                    qx, qy, g.size[0], g.size[1],
                    g.uv[0], g.uv[1], g.uv[2], g.uv[3],
                    color,
                );
            }
            pen_x += g.advance;
        }
        pen_x
    }

    pub fn upload_icon(
        &mut self,
        gl: &glow::Context,
        key: &str,
        rgba_premult: &[u8],
        w: u32,
        h: u32,
    ) -> Option<IconHandle> {
        if let Some(handle) = self.atlas.icons.get(key) {
            return Some(*handle);
        }
        let handle = self.atlas.upload_icon(gl, rgba_premult, w, h)?;
        self.atlas.icons.insert(key.to_string(), handle);
        Some(handle)
    }

    pub fn get_icon(&self, key: &str) -> Option<IconHandle> {
        self.atlas.icons.get(key).copied()
    }

    pub fn draw_icon(&mut self, x: f32, y: f32, w: f32, h: f32, handle: IconHandle) {
        self.push_quad(
            x, y, w, h,
            handle.uv[0], handle.uv[1], handle.uv[2], handle.uv[3],
            [1.0, 1.0, 1.0, 1.0],
        );
    }

    pub fn finish(&mut self, gl: &glow::Context) {
        if self.vertices.is_empty() {
            return;
        }
        unsafe {
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            let bytes = std::slice::from_raw_parts(
                self.vertices.as_ptr() as *const u8,
                self.vertices.len() * std::mem::size_of::<Vertex>(),
            );
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);

            gl.use_program(Some(self.program));
            gl.uniform_2_f32(Some(&self.u_screen), self.screen_w, self.screen_h);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.atlas.texture));
            gl.bind_vertex_array(Some(self.vao));
            gl.draw_arrays(glow::TRIANGLES, 0, self.vertices.len() as i32);
            gl.bind_vertex_array(None);
            gl.use_program(None);
        }
    }

    fn push_quad(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        u0: f32,
        v0: f32,
        u1: f32,
        v1: f32,
        color: Color,
    ) {
        // Pre-multiply on the CPU so the shader is a plain modulate.
        let a = color[3];
        let pre = [color[0] * a, color[1] * a, color[2] * a, a];
        let v = |px, py, u, vu| Vertex {
            pos: [px, py],
            uv: [u, vu],
            color: pre,
        };
        let tl = v(x, y, u0, v0);
        let tr = v(x + w, y, u1, v0);
        let bl = v(x, y + h, u0, v1);
        let br = v(x + w, y + h, u1, v1);
        self.vertices.extend_from_slice(&[tl, bl, br, tl, br, tr]);
    }
}

unsafe fn compile_program(
    gl: &glow::Context,
    vs: &str,
    fs: &str,
) -> Result<glow::Program, String> {
    let p = gl.create_program()?;
    let vsh = compile_shader(gl, glow::VERTEX_SHADER, vs)?;
    let fsh = compile_shader(gl, glow::FRAGMENT_SHADER, fs)?;
    gl.attach_shader(p, vsh);
    gl.attach_shader(p, fsh);
    gl.link_program(p);
    if !gl.get_program_link_status(p) {
        return Err(gl.get_program_info_log(p));
    }
    gl.detach_shader(p, vsh);
    gl.detach_shader(p, fsh);
    gl.delete_shader(vsh);
    gl.delete_shader(fsh);
    Ok(p)
}

unsafe fn compile_shader(gl: &glow::Context, ty: u32, src: &str) -> Result<glow::Shader, String> {
    let s = gl.create_shader(ty)?;
    gl.shader_source(s, src);
    gl.compile_shader(s);
    if !gl.get_shader_compile_status(s) {
        return Err(gl.get_shader_info_log(s));
    }
    Ok(s)
}

impl Atlas {
    fn new(gl: &glow::Context, font: fontdue::Font) -> Result<Self, Box<dyn Error>> {
        unsafe {
            let texture = gl.create_texture().map_err(|e| format!("texture: {e}"))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);

            let zeros = vec![0u8; (ATLAS_W * ATLAS_H * 4) as usize];
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                ATLAS_W as i32,
                ATLAS_H as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                Some(&zeros),
            );

            // Reserve a 2x2 white block at (0,0). Premultiplied opaque white.
            let white: [u8; 16] = [
                255, 255, 255, 255, 255, 255, 255, 255,
                255, 255, 255, 255, 255, 255, 255, 255,
            ];
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                2,
                2,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(&white),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);

            let cu = 0.5 / ATLAS_W as f32;
            let cv = 0.5 / ATLAS_H as f32;
            let white_uv = [cu, cv, cu, cv];

            Ok(Self {
                texture,
                cursor_x: 4,
                cursor_y: 0,
                row_height: 2,
                white_uv,
                glyphs: HashMap::new(),
                icons: HashMap::new(),
                font,
            })
        }
    }

    // Reserves a (w + pad)x(h + pad) cell on the atlas shelf and returns its
    // origin in pixels. Returns None when the atlas is full.
    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let pad = 1u32;
        if w + pad * 2 > ATLAS_W {
            return None;
        }
        if self.cursor_x + w + pad * 2 > ATLAS_W {
            self.cursor_x = 0;
            self.cursor_y += self.row_height + pad * 2;
            self.row_height = 0;
        }
        if self.cursor_y + h + pad * 2 > ATLAS_H {
            return None;
        }
        let x = self.cursor_x + pad;
        let y = self.cursor_y + pad;
        self.cursor_x += w + pad * 2;
        if h > self.row_height {
            self.row_height = h;
        }
        Some((x, y))
    }

    fn upload_glyph(&mut self, gl: &glow::Context, ch: char, size: f32) -> GlyphMetrics {
        let (metrics, bitmap) = self.font.rasterize(ch, size);
        let advance = metrics.advance_width;
        if metrics.width == 0 || metrics.height == 0 {
            return GlyphMetrics::empty(advance);
        }
        let gw = metrics.width as u32;
        let gh = metrics.height as u32;
        let Some((x, y)) = self.alloc(gw, gh) else {
            log::warn!("glyph atlas full");
            return GlyphMetrics::empty(advance);
        };
        // Store coverage in all 4 channels — atlas convention is premult RGBA,
        // and a glyph "premult of opaque white at coverage" is (c,c,c,c).
        let mut rgba = Vec::with_capacity((gw * gh * 4) as usize);
        for &a in &bitmap {
            rgba.extend_from_slice(&[a, a, a, a]);
        }
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                x as i32,
                y as i32,
                gw as i32,
                gh as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(&rgba),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        let u0 = x as f32 / ATLAS_W as f32;
        let v0 = y as f32 / ATLAS_H as f32;
        let u1 = (x + gw) as f32 / ATLAS_W as f32;
        let v1 = (y + gh) as f32 / ATLAS_H as f32;
        GlyphMetrics {
            uv: [u0, v0, u1, v1],
            size: [gw as f32, gh as f32],
            bearing_x: metrics.xmin as f32,
            bearing_ymin: metrics.ymin,
            advance,
        }
    }

    fn upload_icon(&mut self, gl: &glow::Context, rgba: &[u8], w: u32, h: u32) -> Option<IconHandle> {
        if rgba.len() != (w * h * 4) as usize {
            log::warn!("icon size mismatch: {} vs {}x{}x4", rgba.len(), w, h);
            return None;
        }
        let (x, y) = self.alloc(w, h)?;
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(self.texture));
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 4);
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                x as i32,
                y as i32,
                w as i32,
                h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(rgba),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
        let u0 = x as f32 / ATLAS_W as f32;
        let v0 = y as f32 / ATLAS_H as f32;
        let u1 = (x + w) as f32 / ATLAS_W as f32;
        let v1 = (y + h) as f32 / ATLAS_H as f32;
        Some(IconHandle {
            uv: [u0, v0, u1, v1],
            size: [w as f32, h as f32],
        })
    }
}
