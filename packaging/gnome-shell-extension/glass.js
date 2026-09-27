// Liquid Glass under Sysi's cards.
//
// Sysi publishes each GLASS card's rounded rectangle over D-Bus. Every card
// gets an actor inside Sysi's own window actor, below its surface, so the
// glass moves, hides and stacks with the window for free and Sysi's text is
// always painted on top of it. Each actor copies what the compositor has
// already painted behind it, blurs that, and composites the glass in one
// fragment shader.
//
// Two traps on GNOME 50.1 shape the code:
// - Any GI call that hands back an Mtk.Rectangle through an out argument
//   (Mtk.Region.get_extents/get_rectangle, protocol_to_stage_rect) crashes the
//   shell inside g_memdup2, so regions are only ever asked contains_rectangle.
// - Clutter only repaints the damaged part of a frame. Outside that clip the
//   framebuffer still holds last frame's picture, glass included, so copying
//   it would feed the glass back into itself.

import Clutter from 'gi://Clutter';
import Cogl from 'gi://Cogl';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Meta from 'gi://Meta';
import Mtk from 'gi://Mtk';
import St from 'gi://St';

const BUS_NAME = 'io.sysi.Glass';
const OBJECT_PATH = '/io/sysi/Glass';
const IFACE_XML = `
<node>
  <interface name="io.sysi.Glass1">
    <method name="SetCards">
      <arg type="t" direction="in" name="xid"/>
      <arg type="d" direction="in" name="width"/>
      <arg type="d" direction="in" name="height"/>
      <arg type="a(sdddddb)" direction="in" name="cards"/>
      <arg type="b" direction="out" name="attached"/>
    </method>
  </interface>
</node>`;

const MAX_CARDS = 256;
const MAX_SIDE = 16384;
// Room around a card for its shadow and for the samples its rim bends in.
const MARGIN = 32;
// Dual Kawase spread, in texels of the level being read. With four halvings
// on a 2x display (three on 1x) this frosts about as far as Apple's glass.
const KAWASE_OFFSET = 1.5;
const APPEAR_MS = 280;
const PRESS_IN_MS = 150;
const PRESS_OUT_MS = 350;
// A rect update waits for the window's next commit so glass and text move in
// the same frame; this is how long it waits before giving up on one.
const LATCH_TIMEOUT_MS = 50;

const SHADER_COMMON = `
float glass_luma(vec3 c) { return dot(c, vec3(0.2126, 0.7152, 0.0722)); }
`;

// Dual Kawase (Marius Bjorge, SIGGRAPH 2015): halve the picture a few times
// with a five-tap filter, then double it back with an eight-tap one. Every
// tap lands between texels, so each pass blends a wide, even patch; a
// Gaussian with taps a few texels apart instead leaves a grid of ghost
// copies over text. uHalf is half a texel of the level read, times the
// spread.
const KAWASE_DECLARATIONS = `
uniform vec2 uHalf;
`;
const KAWASE_DOWN = `
vec2 uv = cogl_tex_coord0_in.st;
vec4 sum = texture2D(cogl_sampler0, uv) * 4.0;
sum += texture2D(cogl_sampler0, uv - uHalf);
sum += texture2D(cogl_sampler0, uv + uHalf);
sum += texture2D(cogl_sampler0, uv + vec2(uHalf.x, -uHalf.y));
sum += texture2D(cogl_sampler0, uv - vec2(uHalf.x, -uHalf.y));
cogl_color_out = sum / 8.0;
`;
const KAWASE_UP = `
vec2 uv = cogl_tex_coord0_in.st;
vec4 sum = texture2D(cogl_sampler0, uv + vec2(-uHalf.x * 2.0, 0.0));
sum += texture2D(cogl_sampler0, uv + vec2(-uHalf.x, uHalf.y)) * 2.0;
sum += texture2D(cogl_sampler0, uv + vec2(0.0, uHalf.y * 2.0));
sum += texture2D(cogl_sampler0, uv + vec2(uHalf.x, uHalf.y)) * 2.0;
sum += texture2D(cogl_sampler0, uv + vec2(uHalf.x * 2.0, 0.0));
sum += texture2D(cogl_sampler0, uv + vec2(uHalf.x, -uHalf.y)) * 2.0;
sum += texture2D(cogl_sampler0, uv + vec2(0.0, -uHalf.y * 2.0));
sum += texture2D(cogl_sampler0, uv + vec2(-uHalf.x, -uHalf.y)) * 2.0;
cogl_color_out = sum / 12.0;
`;

// Layer 0 is the sharp backdrop, layer 1 the frosted one, layer 2 a lightly
// softened one for the rim; all span the whole actor, card plus margin.
// Every length is in logical pixels.
const GLASS_DECLARATIONS = `${SHADER_COMMON}
uniform vec2 uSize;
uniform vec4 uRect;
uniform float uRadius;
uniform float uScale;
uniform float uAppear;
uniform float uPress;
uniform vec2 uPressAt;

float glass_sd(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;
}

// Outward normal of a rounded rectangle. After Kyant0/AndroidLiquidGlass
// (Apache-2.0): taken on a softer corner than the drawn one so the diagonals
// carry no seam.
vec2 glass_normal(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - (b - r);
    vec2 s = vec2(p.x < 0.0 ? -1.0 : 1.0, p.y < 0.0 ? -1.0 : 1.0);
    if (q.x >= 0.0 || q.y >= 0.0)
        return s * normalize(max(q, 0.0) + 1e-5);
    float gx = step(q.y, q.x);
    return s * vec2(gx, 1.0 - gx);
}

vec3 glass_saturate(vec3 c, float amount) {
    return mix(vec3(glass_luma(c)), c, amount);
}

float glass_linear(float v) { return pow(max(v, 0.0), 2.2); }

float glass_hash(vec2 p) {
    return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453);
}
`;

const GLASS_MAIN = `
vec2 uv = cogl_tex_coord0_in.st;
vec2 px = uv * uSize;
vec2 half_size = uRect.zw * 0.5;
vec2 p = px - (uRect.xy + half_size);
float radius = min(uRadius, min(half_size.x, half_size.y));
float d = glass_sd(p, half_size, radius);
float cover = clamp(0.5 - d * uScale, 0.0, 1.0);
float min_dim = min(uRect.z, uRect.w);
vec3 sharp = texture2D(cogl_sampler0, uv).rgb;
vec3 soft = texture2D(cogl_sampler1, uv).rgb;
vec4 result = vec4(0.0);

if (cover < 1.0) {
    // A fixed, soft shadow. Weighing it by what lay behind the card made it
    // flicker frame to frame while the card was dragged over text.
    float sigma = clamp(min_dim * 0.06, 3.0, 8.0);
    float lift = sigma * 0.35 * uAppear;
    float ds = max(glass_sd(p - vec2(0.0, lift), half_size, radius), 0.0);
    float shade = 0.14 * exp(-(ds * ds) / (2.0 * sigma * sigma));
    // A hairline of darkness right at the rim keeps the edge crisp on light
    // backdrops.
    shade = max(shade, 0.14 * exp(-max(d, 0.0) * uScale));
    result = vec4(0.0, 0.0, 0.0, shade * uAppear * (1.0 - cover));
}

if (cover > 0.0) {
    float depth = max(-d, 0.0);
    float band = min(12.0, 0.2 * min_dim) * uAppear;
    float reach = 1.6 * band;
    vec2 n = glass_normal(p, half_size, min(radius * 1.5, min(half_size.x, half_size.y)));
    n = normalize(n + 0.2 * p / max(length(p), 1e-3));
    // Lensing lives in the rim only; the middle of the card stays flat so
    // nothing swims behind the text.
    float x = band > 0.001 ? clamp(1.0 - depth / band, 0.0, 1.0) : 0.0;
    float bend = (1.0 - sqrt(1.0 - x * x)) * reach;
    vec2 offset = -n * bend / uSize;
    vec3 colour;
    if (bend > 0.05) {
        // The rim bends a crisper picture than the middle frosts, so the
        // lensing reads as lensing and not as more blur.
        vec3 frosted = vec3(texture2D(cogl_sampler1, uv + offset * 0.94).r,
                            texture2D(cogl_sampler1, uv + offset).g,
                            texture2D(cogl_sampler1, uv + offset * 1.06).b);
        vec3 crisp = vec3(texture2D(cogl_sampler2, uv + offset * 0.94).r,
                          texture2D(cogl_sampler2, uv + offset).g,
                          texture2D(cogl_sampler2, uv + offset * 1.06).b);
        colour = mix(crisp, frosted, mix(0.6, 1.0, smoothstep(0.0, band, depth)));
    } else {
        colour = soft;
    }
    // Materialising: with no appearance there is no bending and no frost.
    colour = mix(sharp, colour, uAppear);
    colour = glass_saturate(colour, mix(1.0, 1.5, uAppear));

    // White text must stay at 5:1 or better. Dim just enough for the
    // brightest of this pixel and its neighbourhood, aiming a little under
    // 0.16 so the glow added afterwards cannot tip it over.
    vec2 spread = vec2(20.0) / uSize;
    float lum = glass_linear(glass_luma(colour));
    lum = max(lum, glass_linear(glass_luma(texture2D(cogl_sampler1, uv + spread * vec2(-1.0, 0.0)).rgb)));
    lum = max(lum, glass_linear(glass_luma(texture2D(cogl_sampler1, uv + spread * vec2( 1.0, 0.0)).rgb)));
    lum = max(lum, glass_linear(glass_luma(texture2D(cogl_sampler1, uv + spread * vec2(0.0, -1.0)).rgb)));
    lum = max(lum, glass_linear(glass_luma(texture2D(cogl_sampler1, uv + spread * vec2(0.0,  1.0)).rgb)));
    float keep = pow(clamp(0.14 / max(lum, 1e-4), 0.0, 1.0), 1.0 / 2.2);
    float dim = clamp(max(0.20, 1.0 - keep), 0.0, 0.85) * uAppear;
    colour = colour * (1.0 - dim) + vec3(0.02, 0.02, 0.03) * dim;

    // Rim light from the top left, weaker from the opposite corner, tinted by
    // what lies behind it.
    vec2 light = normalize(vec2(-1.0, -1.0));
    float ring = exp(-depth / 1.0);
    float lit = pow(max(dot(n, light), 0.0), 1.5) + 0.7 * pow(max(dot(n, -light), 0.0), 1.5);
    vec3 tint = mix(vec3(1.0), clamp(glass_saturate(soft, 4.0), 0.0, 1.0), 0.35);
    colour += tint * ring * lit * 0.65 * uAppear;
    colour += 0.035 * exp(-depth / max(0.4 * band, 0.5)) * uAppear;

    // Pressing lights the glass from within, spreading from the pointer.
    vec2 m = px - uPressAt;
    float spot = 0.35 * min_dim;
    colour += uPress * 0.15 * exp(-dot(m, m) / (2.0 * spot * spot));

    colour += (glass_hash(px * uScale) - 0.5) / 255.0;
    colour = clamp(colour, 0.0, 1.0);
    result = vec4(colour * cover, cover) + result * (1.0 - cover);
}
cogl_color_out = result;
`;

function makePipeline(context, declarations, main, layers) {
    const pipeline = Cogl.Pipeline.new(context);
    for (let layer = 0; layer < layers; layer++) {
        pipeline.set_layer_filters(layer,
            Cogl.PipelineFilter.LINEAR, Cogl.PipelineFilter.LINEAR);
        pipeline.set_layer_wrap_mode(layer, Cogl.PipelineWrapMode.CLAMP_TO_EDGE);
    }
    const snippet = Cogl.Snippet.new(Cogl.SnippetHook.FRAGMENT, declarations, '');
    snippet.set_replace(main);
    pipeline.add_snippet(snippet);
    return pipeline;
}

// Uniform locations never change for a pipeline; asking for them every
// frame is a GI round trip per uniform per card.
const uniformLocations = new WeakMap();

function setUniform(pipeline, name, ...values) {
    let locations = uniformLocations.get(pipeline);
    if (!locations) {
        locations = new Map();
        uniformLocations.set(pipeline, locations);
    }
    let location = locations.get(name);
    if (location === undefined) {
        location = pipeline.get_uniform_location(name);
        locations.set(name, location);
    }
    if (location < 0)
        return;
    pipeline.set_uniform_float(location, values.length, 1, values);
}

// Where a point of the actor being painted lands in the framebuffer. By the
// time an effect builds its paint nodes the actor's transform is already on
// the framebuffer's matrix stack, so this holds for any framebuffer: a
// monitor, a screenshot, a screencast.
function framebufferTransform(framebuffer) {
    return {
        // Row-major, points as row vectors: m[row * 4 + column].
        modelview: framebuffer.get_modelview_matrix().to_float(),
        projection: framebuffer.get_projection_matrix().to_float(),
        viewport: framebuffer.get_viewport4fv(),
    };
}

function projectPoint({modelview: m, projection: p, viewport}, x, y) {
    const eye = [0, 1, 2, 3].map(column =>
        x * m[column] + y * m[4 + column] + m[12 + column]);
    const clip = [0, 1, 3].map(column =>
        eye[0] * p[column] + eye[1] * p[4 + column] +
        eye[2] * p[8 + column] + eye[3] * p[12 + column]);
    if (Math.abs(clip[2]) < 1e-9)
        return null;
    return [
        viewport[0] + (clip[0] / clip[2] + 1) / 2 * viewport[2],
        viewport[1] + (1 - clip[1] / clip[2]) / 2 * viewport[3],
    ];
}

class Surface {
    constructor(context, width, height) {
        this.width = width;
        this.height = height;
        this.texture = Cogl.Texture2D.new_with_size(context, width, height);
        this.framebuffer = Cogl.Offscreen.new_with_texture(this.texture);
        this.framebuffer.allocate();
        this.framebuffer.orthographic(0, 0, width, height, -1, 1);
    }
}

// Backdrop copies for one framebuffer: the sharp capture, the halvings the
// blur goes down through, and the doublings it comes back up through. The
// second halving doubles as the lightly softened rim; the last doubling is
// the frosted copy, at half size.
class Backdrop {
    constructor(context, width, height, levels) {
        this.width = width;
        this.height = height;
        this.sharp = new Surface(context, width, height);
        this.down = [];
        for (let level = 1; level <= levels; level++) {
            this.down.push(new Surface(context,
                Math.max(1, Math.ceil(width / 2 ** level)),
                Math.max(1, Math.ceil(height / 2 ** level))));
        }
        // up[i] matches down[i]; the deepest level is only ever read.
        this.up = this.down.slice(0, -1).map(surface =>
            new Surface(context, surface.width, surface.height));
        // A quarter-size copy: soft enough that the rim bends colour rather
        // than chopping every stripe behind it into dashes.
        this.light = this.down[Math.min(1, this.down.length - 1)];
        this.soft = this.up[0];
        this.valid = false;
    }
}

// Enough halvings for the frost to cover the same logical distance on any
// display, but never down to a level with nothing left to blend.
function blurLevels(width, height, scale) {
    let levels = scale >= 1.5 ? 4 : 3;
    while (levels > 2 && Math.min(width, height) / 2 ** levels < 4)
        levels--;
    return levels;
}

const GlassEffect = GObject.registerClass(
class SysiGlassEffect extends Clutter.Effect {
    _init(card) {
        super._init();
        this._card = card;
        // One backdrop per framebuffer drawn into: each monitor, plus
        // whatever transient buffer a screenshot paints the stage to.
        this._backdrops = new Map();
    }

    vfunc_paint_node(node, paintContext) {
        const actor = this.actor;
        if (!actor || actor.is_in_clone_paint())
            return;
        try {
            this._paint(node, paintContext, actor);
        } catch (error) {
            this._card.fail(error);
        }
    }

    _paint(node, paintContext, actor) {
        const framebuffer = paintContext.get_framebuffer();
        // pick_color paints the stage into a 1x1 buffer; glass there is not
        // worth a set of textures.
        if (framebuffer.get_width() < 16 || framebuffer.get_height() < 16)
            return;
        const [stageX, stageY] = actor.get_transformed_position();
        const [stageWidth, stageHeight] = actor.get_transformed_size();
        if (!(stageWidth > 0 && stageHeight > 0))
            return;
        const transform = framebufferTransform(framebuffer);
        const topLeft = projectPoint(transform, 0, 0);
        const bottomRight = projectPoint(transform, actor.width, actor.height);
        if (!topLeft || !bottomRight)
            return;
        const left = Math.floor(topLeft[0]);
        const top = Math.floor(topLeft[1]);
        const width = Math.ceil(bottomRight[0]) - left;
        const height = Math.ceil(bottomRight[1]) - top;
        if (width < 2 || height < 2 || width > MAX_SIDE || height > MAX_SIDE)
            return;
        const scale = width / actor.width;

        const context = framebuffer.get_context();
        const levels = blurLevels(width, height, scale);
        let backdrop = this._backdrops.get(framebuffer);
        if (!backdrop || backdrop.width !== width || backdrop.height !== height ||
            backdrop.down.length !== levels) {
            backdrop = new Backdrop(context, width, height, levels);
            this._backdrops.set(framebuffer, backdrop);
            // A screenshot buffer is thrown away after one paint; do not let
            // them pile up.
            while (this._backdrops.size > 4)
                this._backdrops.delete(this._backdrops.keys().next().value);
        }

        // Only the damaged part of this frame has been repainted underneath;
        // copying beyond it would copy last frame's glass. When the damage
        // covers part of the card, keep the previous copy for now and repaint
        // the whole card next frame.
        const clip = paintContext.get_redraw_clip();
        const overlap = clip
            ? clip.contains_rectangle(new Mtk.Rectangle({
                x: Math.floor(stageX), y: Math.floor(stageY),
                width: Math.ceil(stageWidth), height: Math.ceil(stageHeight),
            }))
            : Mtk.RegionOverlap.IN;
        // A repaint this card asked for itself is taken as whole even if
        // rounding leaves the clip a pixel short, or it would ask forever.
        const forced = this._card.takeForcedRepaint();
        if (overlap === Mtk.RegionOverlap.IN || !backdrop.valid || forced) {
            this._capture(framebuffer, backdrop, left, top);
            backdrop.valid = true;
        } else if (overlap === Mtk.RegionOverlap.PART) {
            this._card.repaintSoon();
        }

        const pipeline = this._card.pipelines(context).glass;
        pipeline.set_layer_texture(0, backdrop.sharp.texture);
        pipeline.set_layer_texture(1, backdrop.soft.texture);
        pipeline.set_layer_texture(2, backdrop.light.texture);
        this._card.setGlassUniforms(pipeline, scale);
        const glass = new Clutter.PipelineNode(pipeline);
        node.add_child(glass);
        glass.add_texture_rectangle(
            new Clutter.ActorBox({x1: 0, y1: 0, x2: actor.width, y2: actor.height}),
            0, 0, 1, 1);
    }

    _capture(framebuffer, backdrop, left, top) {
        // The copy only takes pixels that exist; what falls off the edge of
        // the framebuffer is never seen either.
        const sourceLeft = Math.max(0, left);
        const sourceTop = Math.max(0, top);
        const right = Math.min(framebuffer.get_width(), left + backdrop.width);
        const bottom = Math.min(framebuffer.get_height(), top + backdrop.height);
        if (right <= sourceLeft || bottom <= sourceTop)
            return;
        framebuffer.blit(backdrop.sharp.framebuffer,
            sourceLeft, sourceTop, sourceLeft - left, sourceTop - top,
            right - sourceLeft, bottom - sourceTop);

        // One pipeline per pass: a pass still queued must not see the next
        // one's texture or uniforms.
        const pipelines = this._card.blurPipelines(framebuffer.get_context(),
            backdrop.down.length);
        let source = backdrop.sharp;
        backdrop.down.forEach((target, level) => {
            this._pass(pipelines.down[level], source, target);
            source = target;
        });
        for (let level = backdrop.up.length - 1; level >= 0; level--) {
            const target = backdrop.up[level];
            this._pass(pipelines.up[level], source, target);
            source = target;
        }
    }

    _pass(pipeline, source, target) {
        pipeline.set_layer_texture(0, source.texture);
        setUniform(pipeline, 'uHalf',
            0.5 * KAWASE_OFFSET / source.width, 0.5 * KAWASE_OFFSET / source.height);
        this._draw(target, pipeline);
    }

    // No flush here: Cogl records that the glass samples these textures and
    // flushes their framebuffers first, in order. Flushing each pass by hand
    // made the glass three times as dear on the CPU.
    _draw(surface, pipeline) {
        surface.framebuffer.draw_textured_rectangle(pipeline,
            0, 0, surface.width, surface.height, 0, 0, 1, 1);
    }
});

const GlassCard = GObject.registerClass({
    Properties: {
        'appear': GObject.ParamSpec.double('appear', null, null,
            GObject.ParamFlags.READWRITE, 0, 1, 1),
        'press': GObject.ParamSpec.double('press', null, null,
            GObject.ParamFlags.READWRITE, 0, 1, 0),
    },
}, class SysiGlassCard extends Clutter.Actor {
    _init(host, key) {
        super._init({reactive: false});
        this._host = host;
        this.key = key;
        this._appear = 1;
        this._press = 0;
        this._pressed = false;
        this._pressAt = [0, 0];
        this._rect = [0, 0, 1, 1];
        this._radius = 0;
        this.leaving = false;
        this._forced = false;
        this._pipelines = null;
        this._blur = null;
        this.destroyed = false;
        this.connect('destroy', () => (this.destroyed = true));
        this.add_effect(new GlassEffect(this));
    }

    // Without a paint volume Clutter cannot tell what a redraw of this actor
    // touches and repaints the whole stage for it.
    vfunc_get_paint_volume(volume) {
        return volume.set_from_allocation(this);
    }

    get appear() {
        return this._appear;
    }

    set appear(value) {
        if (this._appear === value)
            return;
        this._appear = value;
        this.notify('appear');
        this.queue_redraw();
    }

    get press() {
        return this._press;
    }

    set press(value) {
        if (this._press === value)
            return;
        this._press = value;
        this.notify('press');
        this.queue_redraw();
    }

    place(x, y, width, height, radius, pressed) {
        this.set_position(x - MARGIN, y - MARGIN);
        this.set_size(width + 2 * MARGIN, height + 2 * MARGIN);
        const changed = this._rect[2] !== width || this._rect[3] !== height ||
            this._radius !== radius;
        this._rect = [MARGIN, MARGIN, width, height];
        this._radius = radius;
        if (pressed !== this._pressed) {
            this._pressed = pressed;
            if (pressed) {
                const [pointerX, pointerY] = global.get_pointer();
                const [ok, localX, localY] = this.transform_stage_point(pointerX, pointerY);
                this._pressAt = ok ? [localX, localY] : [this.width / 2, this.height / 2];
            }
            this.remove_transition('press');
            this.ease_property('press', pressed ? 1 : 0, {
                duration: pressed ? PRESS_IN_MS : PRESS_OUT_MS,
                mode: Clutter.AnimationMode.EASE_OUT_QUAD,
            });
        }
        if (changed)
            this.queue_redraw();
    }

    materialize() {
        this.appear = 0;
        this.ease_property('appear', 1, {
            duration: APPEAR_MS,
            mode: Clutter.AnimationMode.EASE_OUT_QUAD,
        });
    }

    dissolve() {
        this.leaving = true;
        this.remove_transition('appear');
        this.ease_property('appear', 0, {
            duration: APPEAR_MS,
            mode: Clutter.AnimationMode.EASE_OUT_QUAD,
            // Cut short means the card came back or its host went away;
            // either way it is not ours to destroy again.
            onStopped: finished => {
                if (finished && !this.destroyed)
                    this.destroy();
            },
        });
    }

    // Copies of the shared pipelines: each card sets its own uniforms, and a
    // pipeline changed while an earlier card's draw is still queued would
    // repaint that card with this one's shape.
    pipelines(context) {
        this._pipelines ??= {glass: this._host.pipelines(context).glass.copy()};
        return this._pipelines;
    }

    blurPipelines(context, levels) {
        if (this._blur?.down.length !== levels) {
            const {down, up} = this._host.pipelines(context);
            this._blur = {
                down: Array.from({length: levels}, () => down.copy()),
                up: Array.from({length: levels - 1}, () => up.copy()),
            };
        }
        return this._blur;
    }

    takeForcedRepaint() {
        const forced = this._forced;
        this._forced = false;
        return forced;
    }

    forceRepaint() {
        this._forced = true;
        this.queue_redraw();
    }

    setGlassUniforms(pipeline, scale) {
        setUniform(pipeline, 'uSize', this.width, this.height);
        setUniform(pipeline, 'uRect', ...this._rect);
        setUniform(pipeline, 'uRadius', this._radius);
        setUniform(pipeline, 'uScale', scale);
        setUniform(pipeline, 'uAppear', this._appear);
        setUniform(pipeline, 'uPress', this._press);
        setUniform(pipeline, 'uPressAt', ...this._pressAt);
    }

    repaintSoon() {
        this._host.repaintSoon(this);
    }

    fail(error) {
        this._host.fail(error);
    }
});

// The glass for one Sysi window: a layer at the bottom of its window actor,
// holding one card actor per GLASS card.
class GlassHost {
    constructor(manager, windowActor) {
        this._manager = manager;
        this.windowActor = windowActor;
        this._layer = new Clutter.Actor({reactive: false});
        windowActor.insert_child_at_index(this._layer, 0);
        this._cards = new Map();
        this._pending = null;
        this._latchTimeout = 0;
        this._damaged = false;
        this._fresh = true;
        this._pipelines = null;
        this._repaint = new Set();
        this._laterId = 0;
        this._signals = [
            // Xwayland adds the surface actor after the window actor exists,
            // and it has to stay above the glass.
            windowActor.connect('child-added', () => {
                if (this._layer.get_parent() === windowActor)
                    windowActor.set_child_below_sibling(this._layer, null);
            }),
            windowActor.connect('damaged', () => {
                this._damaged = true;
                if (this._pending)
                    this._applyPending();
            }),
            windowActor.connect('destroy', () => this._manager.dropHost(this)),
        ];
        this._afterPaintId = global.stage.connect('after-paint', () => {
            this._damaged = false;
        });
    }

    // Glass and text should change in the same frame. Sysi sends its rects
    // around the time it draws them, so either the commit carrying those
    // pixels has already landed this frame (apply now) or it is still on its
    // way (wait for it).
    setCards(width, height, cards) {
        this._pending = {width, height, cards};
        if (this._fresh || this._damaged) {
            this._applyPending();
            return;
        }
        if (!this._latchTimeout) {
            this._latchTimeout = GLib.timeout_add(GLib.PRIORITY_DEFAULT, LATCH_TIMEOUT_MS, () => {
                this._latchTimeout = 0;
                if (this._pending)
                    this._applyPending();
                return GLib.SOURCE_REMOVE;
            });
        }
    }

    _applyPending() {
        const {width, height, cards} = this._pending;
        this._pending = null;
        if (this._latchTimeout) {
            GLib.source_remove(this._latchTimeout);
            this._latchTimeout = 0;
        }
        const scaleX = this.windowActor.width / width;
        const scaleY = this.windowActor.height / height;
        if (!(scaleX > 0 && scaleY > 0))
            return;
        const animate = !this._fresh;
        this._fresh = false;
        const seen = new Set();
        let below = null;
        for (const [key, x, y, w, h, radius, pressed] of cards) {
            seen.add(key);
            let card = this._cards.get(key);
            if (!card || card.leaving) {
                card?.destroy();
                card = new GlassCard(this, key);
                this._cards.set(key, card);
                card.connect('destroy', () => {
                    if (this._cards.get(key) === card)
                        this._cards.delete(key);
                });
                this._layer.add_child(card);
                if (animate)
                    card.materialize();
            }
            card.place(x * scaleX, y * scaleY, w * scaleX, h * scaleY,
                radius * Math.min(scaleX, scaleY), pressed);
            if (below)
                this._layer.set_child_above_sibling(card, below);
            else
                this._layer.set_child_below_sibling(card, null);
            below = card;
        }
        for (const [key, card] of this._cards) {
            if (seen.has(key) || card.leaving)
                continue;
            if (animate)
                card.dissolve();
            else
                card.destroy();
        }
    }

    pipelines(context) {
        if (!this._pipelines) {
            this._pipelines = {
                down: makePipeline(context, KAWASE_DECLARATIONS, KAWASE_DOWN, 1),
                up: makePipeline(context, KAWASE_DECLARATIONS, KAWASE_UP, 1),
                glass: makePipeline(context, GLASS_DECLARATIONS, GLASS_MAIN, 3),
            };
            // The blur passes overwrite their target outright.
            for (const pipeline of [this._pipelines.down, this._pipelines.up])
                pipeline.set_blend('RGBA = ADD (SRC_COLOR, 0)');
        }
        return this._pipelines;
    }

    repaintSoon(card) {
        this._repaint.add(card);
        if (this._laterId)
            return;
        this._laterId = global.compositor.get_laters().add(Meta.LaterType.BEFORE_REDRAW, () => {
            this._laterId = 0;
            for (const pending of this._repaint) {
                if (!pending.destroyed)
                    pending.forceRepaint();
            }
            this._repaint.clear();
            return GLib.SOURCE_REMOVE;
        });
    }

    fail(error) {
        this._manager.fail(error);
    }

    destroy() {
        if (this._latchTimeout)
            GLib.source_remove(this._latchTimeout);
        this._latchTimeout = 0;
        if (this._laterId)
            global.compositor.get_laters().remove(this._laterId);
        this._laterId = 0;
        global.stage.disconnect(this._afterPaintId);
        if (!this.windowActor.is_destroyed?.()) {
            for (const id of this._signals)
                this.windowActor.disconnect(id);
        }
        this._signals = [];
        this._layer.destroy();
        this._cards.clear();
        this._pipelines = null;
    }
}

export class GlassManager {
    enable() {
        this._hosts = new Map();
        this._state = null;
        this._failed = false;
        this._settings = St.Settings.get();
        this._contrastId = this._settings.connect('notify::high-contrast', () => this._syncOwnership());
        this._mapId = global.window_manager.connect('map', (_wm, actor) => this._reattach(actor));
        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this);
        this._ownerId = 0;
        this._syncOwnership();
    }

    destroy() {
        this._unown();
        this._dbus = null;
        if (this._contrastId)
            this._settings.disconnect(this._contrastId);
        this._contrastId = 0;
        if (this._mapId)
            global.window_manager.disconnect(this._mapId);
        this._mapId = 0;
        this._dropAll();
        this._settings = null;
    }

    // Increase-contrast means solid plates: give up the name and Sysi paints
    // its own.
    _syncOwnership() {
        const wanted = !this._failed && !this._settings.high_contrast;
        if (wanted && !this._ownerId) {
            this._ownerId = Gio.bus_own_name(Gio.BusType.SESSION, BUS_NAME,
                Gio.BusNameOwnerFlags.NONE,
                connection => this._dbus.export(connection, OBJECT_PATH),
                null, null);
        } else if (!wanted && this._ownerId) {
            this._unown();
            this._dropAll();
        }
    }

    _unown() {
        if (this._ownerId) {
            Gio.bus_unown_name(this._ownerId);
            this._ownerId = 0;
        }
        try {
            this._dbus?.unexport();
        } catch (_) {
            // Never exported: the bus was not reached yet.
        }
    }

    SetCards(xid, width, height, cards) {
        if (this._failed || !this._ownerId)
            return false;
        if (!Number.isFinite(width) || !Number.isFinite(height) ||
            width < 1 || height < 1 || width > MAX_SIDE * 4 || height > MAX_SIDE * 4)
            return false;
        const clean = [];
        for (const card of cards.slice(0, MAX_CARDS)) {
            const [key, x, y, w, h, radius, pressed] = card;
            if (typeof key !== 'string' || key.length === 0 || key.length > 64)
                continue;
            if (![x, y, w, h, radius].every(Number.isFinite))
                continue;
            if (w <= 1 || h <= 1 || w > MAX_SIDE || h > MAX_SIDE)
                continue;
            const clampedRadius = Math.max(0, Math.min(radius, w / 2, h / 2));
            clean.push([key, x, y, w, h, clampedRadius, Boolean(pressed)]);
        }
        this._state = {xid, width, height, cards: clean};
        const host = this._hostFor(xid);
        if (!host)
            return false;
        host.setCards(width, height, clean);
        return true;
    }

    _hostFor(xid) {
        const existing = this._hosts.get(xid);
        if (existing && !existing.windowActor.is_destroyed?.())
            return existing;
        const actor = global.get_window_actors().find(candidate =>
            this._matches(candidate, xid));
        if (!actor)
            return null;
        const host = new GlassHost(this, actor);
        this._hosts.set(xid, host);
        return host;
    }

    // Anything on the session bus may call SetCards, so the xid alone is not
    // trusted to name Sysi's overlay: the window has to be it as well.
    _matches(actor, xid) {
        const window = actor.meta_window;
        if (!window || window.get_client_type() !== Meta.WindowClientType.X11)
            return false;
        if (window.get_title() !== 'Sysi Overlay' ||
            (window.get_wm_class() ?? '').toLowerCase() !== 'sysi')
            return false;
        const [token] = (window.get_description() ?? '').split(' ');
        return token === `0x${xid.toString(16)}`;
    }

    // Hiding Sysi unmaps its window and takes the glass with it. Showing it
    // again brings a new window actor and no new rects (none changed), so the
    // last ones Sysi sent are put straight back.
    _reattach(actor) {
        const state = this._state;
        if (!state || this._failed || !this._matches(actor, state.xid))
            return;
        this._hosts.get(state.xid)?.destroy();
        this._hosts.delete(state.xid);
        const host = new GlassHost(this, actor);
        this._hosts.set(state.xid, host);
        host.setCards(state.width, state.height, state.cards);
    }

    dropHost(host) {
        for (const [xid, candidate] of this._hosts) {
            if (candidate === host) {
                this._hosts.delete(xid);
                host.destroy();
            }
        }
    }

    _dropAll() {
        for (const host of this._hosts.values())
            host.destroy();
        this._hosts.clear();
    }

    // A broken shader or a missing API must not take the shell down or leave
    // Sysi's cards clear and unreadable. Stop drawing, let go of the name, and
    // Sysi falls back to painting its own plates.
    fail(error) {
        if (this._failed)
            return;
        this._failed = true;
        logError(error, 'Sysi glass stopped');
        GLib.idle_add(GLib.PRIORITY_DEFAULT, () => {
            this._unown();
            this._dropAll();
            return GLib.SOURCE_REMOVE;
        });
    }
}
