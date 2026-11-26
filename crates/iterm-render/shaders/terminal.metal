#include <metal_stdlib>
using namespace metal;

struct ViewportUniforms {
    float2 viewport_size;   // drawable size in pixels
    float  scale_factor;    // Retina backing scale (1.0 / 2.0)
    float  time_sec;        // monotonic time for cursor blink / visual bell
};

// ---------------------------------------------------------------------------
// Pass 1: Instanced Solid / Rounded / Curly-Underline Quads (32 bytes)
// ---------------------------------------------------------------------------
struct QuadInstance {
    float4 rect;            // x, y, width, height (pixels, top-left origin)
    float4 color;           // RGBA linear/sRGB [0..1]
    uint   mode;            // 0 = solid, 1 = curly underline, 2 = hollow cursor outline, 3 = rounded rect
    float  param;           // corner radius or line thickness in pixels
    float2 _pad;
};

struct QuadOut {
    float4 position [[position]];
    float4 color;
    float2 uv;              // [0..1] across quad
    float2 size_px;
    uint   mode;
    float  param;
};

constant float2 UNIT_QUAD[6] = {
    float2(0.0, 0.0), float2(1.0, 0.0), float2(0.0, 1.0),
    float2(1.0, 0.0), float2(1.0, 1.0), float2(0.0, 1.0),
};

vertex QuadOut quad_vertex(
    uint vid [[vertex_id]],
    uint iid [[instance_id]],
    constant ViewportUniforms &u [[buffer(0)]],
    const device QuadInstance *instances [[buffer(1)]]
) {
    QuadInstance q = instances[iid];
    float2 unit = UNIT_QUAD[vid];
    float2 px = q.rect.xy + unit * q.rect.zw;
    float2 ndc = float2(
        (px.x / u.viewport_size.x) * 2.0 - 1.0,
        1.0 - (px.y / u.viewport_size.y) * 2.0
    );
    QuadOut out;
    out.position = float4(ndc, 0.0, 1.0);
    out.color = q.color;
    out.uv = unit;
    out.size_px = q.rect.zw;
    out.mode = q.mode;
    out.param = q.param;
    return out;
}

fragment float4 quad_fragment(QuadOut in [[stage_in]]) {
    if (in.mode == 0u) {
        return in.color;
    }
    if (in.mode == 1u) {
        // Procedural anti-aliased curly underline (SGR 4:3)
        float2 p = in.uv * in.size_px;
        float amp = max(1.0, in.size_px.y * 0.35);
        float freq = 3.14159265 / max(2.5, in.size_px.y * 0.75);
        float wave_y = in.size_px.y * 0.5 + sin(p.x * freq) * amp;
        float dist = abs(p.y - wave_y);
        float stroke = max(1.0, in.param);
        float alpha = 1.0 - smoothstep(stroke * 0.45, stroke * 0.95, dist);
        return float4(in.color.rgb, in.color.a * alpha);
    }
    if (in.mode == 2u) {
        // Hollow block cursor outline (unfocused pane)
        float2 p = in.uv * in.size_px;
        float b = max(1.0, in.param);
        bool edge = (p.x < b) || (p.x >= in.size_px.x - b) ||
                    (p.y < b) || (p.y >= in.size_px.y - b);
        return edge ? in.color : float4(0.0);
    }
    if (in.mode == 3u) {
        // mode == 3: smooth rounded rectangle (tabs, badges, search highlights)
        float2 p = (in.uv - 0.5) * in.size_px;
        float r = min(in.param, min(in.size_px.x, in.size_px.y) * 0.5);
        float2 q = abs(p) - (in.size_px * 0.5 - float2(r));
        float dist = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
        float alpha = 1.0 - smoothstep(-0.75, 0.75, dist);
        return float4(in.color.rgb, in.color.a * alpha);
    }
    if (in.mode == 4u) {
        // Powerline right-pointing solid triangle U+E0B0 (): pixel-exact anti-aliased edge
        float edge_x = (1.0 - abs(2.0 * in.uv.y - 1.0)) * in.size_px.x;
        float px = in.uv.x * in.size_px.x;
        float alpha = 1.0 - smoothstep(-0.6, 0.6, px - edge_x);
        return float4(in.color.rgb, in.color.a * alpha);
    }
    if (in.mode == 5u) {
        // Powerline left-pointing solid triangle U+E0B2 ()
        float edge_x = abs(2.0 * in.uv.y - 1.0) * in.size_px.x;
        float px = in.uv.x * in.size_px.x;
        float alpha = 1.0 - smoothstep(-0.6, 0.6, edge_x - px);
        return float4(in.color.rgb, in.color.a * alpha);
    }
    if (in.mode == 6u) {
        // Powerline right half-circle pill cap U+E0B4 ()
        float2 d = float2(in.uv.x, in.uv.y * 2.0 - 1.0);
        float dist = (length(d) - 1.0) * min(in.size_px.x, in.size_px.y * 0.5);
        float alpha = 1.0 - smoothstep(-0.6, 0.6, dist);
        return float4(in.color.rgb, in.color.a * alpha);
    }
    // mode == 7u: Powerline left half-circle pill cap U+E0B6 ()
    float2 d = float2(1.0 - in.uv.x, in.uv.y * 2.0 - 1.0);
    float dist = (length(d) - 1.0) * min(in.size_px.x, in.size_px.y * 0.5);
    float alpha = 1.0 - smoothstep(-0.6, 0.6, dist);
    return float4(in.color.rgb, in.color.a * alpha);
}

// ---------------------------------------------------------------------------
// Pass 2: Instanced Glyph Atlas Text (48 bytes)
// ---------------------------------------------------------------------------
struct GlyphInstance {
    float4 dst_rect;        // x, y, width, height (pixels)
    float4 uv_rect;         // u0, v0, u1, v1 (normalized [0..1] atlas UVs)
    float4 fg_color;        // foreground RGBA (ignored when is_color == 1)
    uint   flags;           // bit 0: is_color (emoji RGBA), bit 1: faint/dim
    uint   _pad0;
    uint   _pad1;
    uint   _pad2;
};

struct GlyphOut {
    float4 position [[position]];
    float2 uv;
    float4 fg_color;
    uint   flags;
};

vertex GlyphOut glyph_vertex(
    uint vid [[vertex_id]],
    uint iid [[instance_id]],
    constant ViewportUniforms &u [[buffer(0)]],
    const device GlyphInstance *instances [[buffer(1)]]
) {
    GlyphInstance g = instances[iid];
    float2 unit = UNIT_QUAD[vid];
    float2 px = g.dst_rect.xy + unit * g.dst_rect.zw;
    float2 ndc = float2(
        (px.x / u.viewport_size.x) * 2.0 - 1.0,
        1.0 - (px.y / u.viewport_size.y) * 2.0
    );
    GlyphOut out;
    out.position = float4(ndc, 0.0, 1.0);
    out.uv = mix(g.uv_rect.xy, g.uv_rect.zw, unit);
    out.fg_color = g.fg_color;
    out.flags = g.flags;
    return out;
}

fragment float4 glyph_fragment(
    GlyphOut in [[stage_in]],
    texture2d<float> mono_atlas [[texture(0)]],
    texture2d<float> color_atlas [[texture(1)]],
    sampler samp [[sampler(0)]]
) {
    if ((in.flags & 1u) != 0u) {
        // Full-color bitmap glyph (Apple Color Emoji) from RGBA8 color atlas
        float4 tex = color_atlas.sample(samp, in.uv);
        return float4(tex.rgb, tex.a * in.fg_color.a);
    }
    // Single-channel R8Unorm grayscale coverage from 4096x4096 monochrome text atlas.
    // Foreground-luminance-aware perceptual gamma: boosts crisp stem weight on light-on-dark
    // themes (Dracula+) without over-bolding dark-on-light themes or white TUI bars!
    float cov = mono_atlas.sample(samp, in.uv).r;
    if (cov <= 0.002) {
        discard_fragment();
    }
    float lum = dot(in.fg_color.rgb, float3(0.2126, 0.7152, 0.0722));
    float gamma = mix(1.08, 0.80, clamp(lum, 0.0, 1.0));
    cov = pow(cov, gamma);
    return float4(in.fg_color.rgb, in.fg_color.a * cov);
}

// ---------------------------------------------------------------------------
// Pass 3: Inline Images (OSC 1337 imgcat)
// ---------------------------------------------------------------------------
struct ImageVertexOut {
    float4 position [[position]];
    float2 uv;
};

struct ImageUniforms {
    float4 dst_rect;        // x, y, width, height (pixels)
    float4 uv_rect;         // u0, v0, u1, v1
    float  alpha;
    float3 _pad;
};

vertex ImageVertexOut image_vertex(
    uint vid [[vertex_id]],
    constant ViewportUniforms &u [[buffer(0)]],
    constant ImageUniforms &img [[buffer(1)]]
) {
    float2 unit = UNIT_QUAD[vid];
    float2 px = img.dst_rect.xy + unit * img.dst_rect.zw;
    float2 ndc = float2(
        (px.x / u.viewport_size.x) * 2.0 - 1.0,
        1.0 - (px.y / u.viewport_size.y) * 2.0
    );
    ImageVertexOut out;
    out.position = float4(ndc, 0.0, 1.0);
    out.uv = mix(img.uv_rect.xy, img.uv_rect.zw, unit);
    return out;
}

fragment float4 image_fragment(
    ImageVertexOut in [[stage_in]],
    constant ImageUniforms &img [[buffer(1)]],
    texture2d<float> tex [[texture(0)]],
    sampler samp [[sampler(0)]]
) {
    float4 c = tex.sample(samp, in.uv);
    return float4(c.rgb, c.a * img.alpha);
}
