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
// A card the damage only partly covered (a caret, a line of typing, a card
// dragged across it) keeps its last copy and is copied whole this long
// after. The glass is frosted: a moment's lag behind what moves under it
// does not show, and copying and blurring a large card for every keystroke
// did.
const REPAINT_DELAY_MS = 120;
// How long a screenshot's or recording's spare backdrop outlives its use.
const SPARE_KEEP_MS = 2000;

const SHADER_COMMON = `
float glass_luma(vec3 c) { return dot(c, vec3(0.2126, 0.7152, 0.0722)); }
`;

// Dual Kawase (Marius Bjorge, SIGGRAPH 2015): halve the picture a few times
// with a five-tap filter, then double it back with an eight-tap one. Every
// tap lands between texels, so each pass blends a wide, even patch; a
// Gaussian with taps a few texels apart instead leaves a grid of ghost
// copies over text. uHalf is half a texel of the level read, times the
// spread. uMax is the last texel centre of the part this frame's copy
// fills: the textures are bigger than the copy, and CLAMP_TO_EDGE only
// stops at the texture's own edge.
const KAWASE_DECLARATIONS = `
uniform vec2 uHalf;
uniform vec2 uMax;
vec4 kawase_tap(vec2 uv) { return texture2D(cogl_sampler0, min(uv, uMax)); }
`;
const KAWASE_DOWN = `
vec2 uv = cogl_tex_coord0_in.st;
vec4 sum = kawase_tap(uv) * 4.0;
sum += kawase_tap(uv - uHalf);
sum += kawase_tap(uv + uHalf);
sum += kawase_tap(uv + vec2(uHalf.x, -uHalf.y));
sum += kawase_tap(uv - vec2(uHalf.x, -uHalf.y));
cogl_color_out = sum / 8.0;
`;
const KAWASE_UP = `
vec2 uv = cogl_tex_coord0_in.st;
vec4 sum = kawase_tap(uv + vec2(-uHalf.x * 2.0, 0.0));
sum += kawase_tap(uv + vec2(-uHalf.x, uHalf.y)) * 2.0;
sum += kawase_tap(uv + vec2(0.0, uHalf.y * 2.0));
sum += kawase_tap(uv + vec2(uHalf.x, uHalf.y)) * 2.0;
sum += kawase_tap(uv + vec2(uHalf.x * 2.0, 0.0));
sum += kawase_tap(uv + vec2(uHalf.x, -uHalf.y)) * 2.0;
sum += kawase_tap(uv + vec2(0.0, -uHalf.y * 2.0));
sum += kawase_tap(uv + vec2(-uHalf.x, -uHalf.y)) * 2.0;
cogl_color_out = sum / 12.0;
`;

// Layer 0 is the sharp backdrop, layer 1 the frosted one, layer 2 a lightly
// softened one for the rim; all span the whole actor, card plus margin.
// Every length is in logical pixels.
const GLASS_DECLARATIONS = `${SHADER_COMMON}
uniform vec2 uSize;
uniform vec2 uUvScale;
// The last texel centre of the copy in the coarsest layer: nothing past it
// belongs to this frame.
uniform vec2 uUvMax;
uniform vec4 uRect;
uniform float uRadius;
uniform float uScale;
uniform float uAppear;
// 1 for a card's lit rim, 0 for a popup's plain edge.
uniform float uRim;
// The actor's paint opacity: a shell menu fades in and out with its own.
uniform float uOpacity;
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

vec2 glass_uv(vec2 uv) { return min(uv, uUvMax); }

float glass_linear(float v) { return pow(max(v, 0.0), 2.2); }

float glass_hash(vec2 p) {
    return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453);
}
`;

const GLASS_MAIN = `
// The backdrop textures are allocated with room to spare; uUvScale is the
// share of them this frame's copy fills.
vec2 local = cogl_tex_coord0_in.st;
vec2 uv = local * uUvScale;
vec2 px = local * uSize;
vec2 to_tex = uUvScale / uSize;
vec2 half_size = uRect.zw * 0.5;
vec2 p = px - (uRect.xy + half_size);
float radius = min(uRadius, min(half_size.x, half_size.y));
float d = glass_sd(p, half_size, radius);
float cover = clamp(0.5 - d * uScale, 0.0, 1.0);
float min_dim = min(uRect.z, uRect.w);
vec3 sharp = texture2D(cogl_sampler0, glass_uv(uv)).rgb;
vec3 soft = texture2D(cogl_sampler1, glass_uv(uv)).rgb;
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
    shade = max(shade, 0.14 * uRim * exp(-max(d, 0.0) * uScale));
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
    vec2 offset = -n * bend * to_tex;
    vec3 colour;
    if (bend > 0.05) {
        // The rim bends a crisper picture than the middle frosts, so the
        // lensing reads as lensing and not as more blur.
        vec3 frosted = vec3(texture2D(cogl_sampler1, glass_uv(uv + offset * 0.94)).r,
                            texture2D(cogl_sampler1, glass_uv(uv + offset)).g,
                            texture2D(cogl_sampler1, glass_uv(uv + offset * 1.06)).b);
        vec3 crisp = vec3(texture2D(cogl_sampler2, glass_uv(uv + offset * 0.94)).r,
                          texture2D(cogl_sampler2, glass_uv(uv + offset)).g,
                          texture2D(cogl_sampler2, glass_uv(uv + offset * 1.06)).b);
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
    vec2 spread = vec2(20.0) * to_tex;
    float lum = glass_linear(glass_luma(colour));
    lum = max(lum, glass_linear(glass_luma(texture2D(cogl_sampler1, glass_uv(uv + spread * vec2(-1.0, 0.0))).rgb)));
    lum = max(lum, glass_linear(glass_luma(texture2D(cogl_sampler1, glass_uv(uv + spread * vec2( 1.0, 0.0))).rgb)));
    lum = max(lum, glass_linear(glass_luma(texture2D(cogl_sampler1, glass_uv(uv + spread * vec2(0.0, -1.0))).rgb)));
    lum = max(lum, glass_linear(glass_luma(texture2D(cogl_sampler1, glass_uv(uv + spread * vec2(0.0,  1.0))).rgb)));
    float keep = pow(clamp(0.14 / max(lum, 1e-4), 0.0, 1.0), 1.0 / 2.2);
    float dim = clamp(max(0.20, 1.0 - keep), 0.0, 0.85) * uAppear;
    colour = colour * (1.0 - dim) + vec3(0.02, 0.02, 0.03) * dim;

    // Rim light from the top left, weaker from the opposite corner, tinted by
    // what lies behind it.
    vec2 light = normalize(vec2(-1.0, -1.0));
    float ring = exp(-depth / 1.0);
    float lit = pow(max(dot(n, light), 0.0), 1.5) + 0.7 * pow(max(dot(n, -light), 0.0), 1.5);
    vec3 tint = mix(vec3(1.0), clamp(glass_saturate(soft, 4.0), 0.0, 1.0), 0.35);
    colour += tint * ring * lit * 0.65 * uAppear * uRim;
    colour += 0.035 * exp(-depth / max(0.4 * band, 0.5)) * uAppear * uRim;

    // Pressing lights the glass from within, spreading from the pointer.
    vec2 m = px - uPressAt;
    float spot = min(0.18 * min_dim, 40.0);
    colour += uPress * 0.12 * exp(-dot(m, m) / (2.0 * spot * spot));

    colour += (glass_hash(px * uScale) - 0.5) / 255.0;
    colour = clamp(colour, 0.0, 1.0);
    result = vec4(colour * cover, cover) + result * (1.0 - cover);
}
cogl_color_out = result * uOpacity;
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

function makePipelines(context) {
    const pipelines = {
        down: makePipeline(context, KAWASE_DECLARATIONS, KAWASE_DOWN, 1),
        up: makePipeline(context, KAWASE_DECLARATIONS, KAWASE_UP, 1),
        glass: makePipeline(context, GLASS_DECLARATIONS, GLASS_MAIN, 3),
    };
    // The blur passes overwrite their target outright.
    for (const pipeline of [pipelines.down, pipelines.up])
        pipeline.set_blend('RGBA = ADD (SRC_COLOR, 0)');
    return pipelines;
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
//
// The textures are sized up to a multiple of BACKDROP_STEP and reused for as
// long as the card fits: a card being dragged lands on half pixels, so the
// copy it needs grows and shrinks by a pixel frame to frame, and allocating
// afresh each time left a whole set of large textures behind every frame
// until the shell next collected garbage (a gigabyte for a large note).
class Backdrop {
    constructor(context, width, height, levels) {
        this.width = width;
        this.height = height;
        // The part of the textures the current copy covers.
        this.usedWidth = width;
        this.usedHeight = height;
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

const BACKDROP_STEP = 256;

// Room for the card to grow into: a quarter more than it needs, so a card
// being resized outgrows its textures a handful of times, not every frame.
function backdropSize(needed) {
    return Math.ceil(needed * 1.25 / BACKDROP_STEP) * BACKDROP_STEP;
}

// Whether a backdrop allocated for one size still serves another: big
// enough, and not so much bigger that it wastes the memory.
function backdropFits(backdrop, width, height, levels) {
    return backdrop.down.length === levels &&
        width <= backdrop.width && height <= backdrop.height &&
        backdrop.width < width * 1.6 + BACKDROP_STEP &&
        backdrop.height < height * 1.6 + BACKDROP_STEP;
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
        // One backdrop per monitor framebuffer drawn into.
        this._backdrops = new Map();
        this._spare = null;
        this._spareUsed = 0;
        this._spareTimeout = 0;
    }

    // The glass, then whatever the actor paints itself: nothing for a card,
    // the items for a shell menu.
    vfunc_paint_node(node, paintContext, flags) {
        const actor = this.actor;
        if (actor && !actor.is_in_clone_paint()) {
            try {
                this._paint(node, paintContext, actor);
            } catch (error) {
                this._card.fail(error);
            }
        }
        super.vfunc_paint_node(node, paintContext, flags);
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

        // The part of the actor on this framebuffer, in stage coordinates,
        // shrunk to whole pixels. A monitor's redraw clip never reaches past
        // the monitor, so asking it to hold the whole actor would answer PART
        // for any card near an edge, and for one across two monitors ask for
        // a repaint every frame, forever.
        const fbWidth = framebuffer.get_width();
        const fbHeight = framebuffer.get_height();
        const toStageX = stageWidth / (bottomRight[0] - topLeft[0]);
        const toStageY = stageHeight / (bottomRight[1] - topLeft[1]);
        const visible = {
            x1: Math.ceil(stageX + (Math.max(0, topLeft[0]) - topLeft[0]) * toStageX),
            y1: Math.ceil(stageY + (Math.max(0, topLeft[1]) - topLeft[1]) * toStageY),
            x2: Math.floor(stageX + (Math.min(fbWidth, bottomRight[0]) - topLeft[0]) * toStageX),
            y2: Math.floor(stageY + (Math.min(fbHeight, bottomRight[1]) - topLeft[1]) * toStageY),
        };
        if (visible.x2 <= visible.x1 || visible.y2 <= visible.y1)
            return;

        const context = framebuffer.get_context();
        const levels = blurLevels(width, height, scale);
        const backdrop = this._backdropFor(framebuffer, context, width, height, levels);
        backdrop.usedWidth = width;
        backdrop.usedHeight = height;

        // Only the damaged part of this frame has been repainted underneath;
        // copying beyond it would copy last frame's glass. When the damage
        // covers part of the card, keep the previous copy for now and repaint
        // the whole card shortly.
        const clip = paintContext.get_redraw_clip();
        const overlap = clip
            ? clip.contains_rectangle(new Mtk.Rectangle({
                x: visible.x1, y: visible.y1,
                width: visible.x2 - visible.x1, height: visible.y2 - visible.y1,
            }))
            : Mtk.RegionOverlap.IN;
        // A repaint this card asked for itself is taken as whole even if
        // rounding leaves the clip a pixel short, or it would ask forever.
        if (overlap === Mtk.RegionOverlap.IN || !backdrop.valid || this._card.forced) {
            this._capture(framebuffer, backdrop, left, top);
            backdrop.valid = true;
        } else if (overlap === Mtk.RegionOverlap.PART) {
            this._card.repaintSoon();
        }

        const pipeline = this._card.pipelines(context).glass;
        pipeline.set_layer_texture(0, backdrop.sharp.texture);
        pipeline.set_layer_texture(1, backdrop.soft.texture);
        pipeline.set_layer_texture(2, backdrop.light.texture);
        this._card.setGlassUniforms(pipeline, scale,
            width / backdrop.width, height / backdrop.height,
            (width / backdrop.width) - 0.5 / backdrop.light.width,
            (height / backdrop.height) - 0.5 / backdrop.light.height);
        const glass = new Clutter.PipelineNode(pipeline);
        node.add_child(glass);
        glass.add_texture_rectangle(
            new Clutter.ActorBox({x1: 0, y1: 0, x2: actor.width, y2: actor.height}),
            0, 0, 1, 1);
    }

    // A monitor's framebuffer is painted every frame, so its copies are kept,
    // and dropped once the monitor's view is gone (a hotplug, a new scale or
    // layout): otherwise each change left a set behind for good. Anything
    // else (a screenshot, a screen recording, a screencast) shares one spare
    // set, since a recording can paint into a new framebuffer every frame;
    // the spare is let go once nothing has painted with it for a while.
    _backdropFor(framebuffer, context, width, height, levels) {
        const views = global.stage.peek_stage_views().map(view => view.get_framebuffer());
        if (views.includes(framebuffer)) {
            let backdrop = this._backdrops.get(framebuffer);
            if (!backdrop || !backdropFits(backdrop, width, height, levels)) {
                for (const key of this._backdrops.keys()) {
                    if (!views.includes(key))
                        this._backdrops.delete(key);
                }
                backdrop = new Backdrop(context,
                    backdropSize(width), backdropSize(height), levels);
                this._backdrops.set(framebuffer, backdrop);
            }
            return backdrop;
        }
        if (!this._spare || !backdropFits(this._spare, width, height, levels)) {
            this._spare = new Backdrop(context,
                backdropSize(width), backdropSize(height), levels);
        }
        this._spareUsed = GLib.get_monotonic_time();
        this._spareTimeout ||= GLib.timeout_add(GLib.PRIORITY_DEFAULT, SPARE_KEEP_MS, () => {
            if (GLib.get_monotonic_time() - this._spareUsed < SPARE_KEEP_MS * 1000)
                return GLib.SOURCE_CONTINUE;
            this._spare = null;
            this._spareTimeout = 0;
            return GLib.SOURCE_REMOVE;
        });
        // Every paint here is its own picture: never trust a spare's copy.
        this._spare.valid = false;
        return this._spare;
    }

    _capture(framebuffer, backdrop, left, top) {
        // The copy only takes pixels that exist; what falls off the edge of
        // the framebuffer is never seen either.
        const sourceLeft = Math.max(0, left);
        const sourceTop = Math.max(0, top);
        const right = Math.min(framebuffer.get_width(), left + backdrop.usedWidth);
        const bottom = Math.min(framebuffer.get_height(), top + backdrop.usedHeight);
        if (right <= sourceLeft || bottom <= sourceTop)
            return;
        try {
            framebuffer.blit(backdrop.sharp.framebuffer,
                sourceLeft, sourceTop, sourceLeft - left, sourceTop - top,
                right - sourceLeft, bottom - sourceTop);
        } catch (_) {
            // A framebuffer whose pixels cannot be copied into ours (a
            // floating-point HDR screenshot, say) just keeps the last copy;
            // it is no reason to give up the glass everywhere else.
            return;
        }

        // One pipeline per pass: a pass still queued must not see the next
        // one's texture or uniforms.
        const pipelines = this._card.blurPipelines(framebuffer.get_context(),
            backdrop.down.length);
        // Every level is the same share of its texture, since each is an
        // exact halving of a multiple of BACKDROP_STEP.
        const share = [backdrop.usedWidth / backdrop.width,
            backdrop.usedHeight / backdrop.height];
        let source = backdrop.sharp;
        backdrop.down.forEach((target, level) => {
            this._pass(pipelines.down[level], source, target, share);
            source = target;
        });
        for (let level = backdrop.up.length - 1; level >= 0; level--) {
            const target = backdrop.up[level];
            this._pass(pipelines.up[level], source, target, share);
            source = target;
        }
    }

    // No flush here: Cogl records that the glass samples these textures and
    // flushes their framebuffers first, in order. Flushing each pass by hand
    // made the glass three times as dear on the CPU.
    _pass(pipeline, source, target, [shareX, shareY]) {
        pipeline.set_layer_texture(0, source.texture);
        setUniform(pipeline, 'uHalf',
            0.5 * KAWASE_OFFSET / source.width, 0.5 * KAWASE_OFFSET / source.height);
        setUniform(pipeline, 'uMax',
            shareX - 0.5 / source.width, shareY - 0.5 / source.height);
        target.framebuffer.draw_textured_rectangle(pipeline,
            0, 0, target.width * shareX, target.height * shareY,
            0, 0, shareX, shareY);
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
        // A context menu or a popover sits on glass with no lit edge.
        this._rim = key === 'menu' || key.startsWith('popover:') ? 0 : 1;
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
        let changed = this._rect[2] !== width || this._rect[3] !== height ||
            this._radius !== radius;
        this._rect = [MARGIN, MARGIN, width, height];
        this._radius = radius;
        // The light stays under the pointer while the card is held. Dragged,
        // the card carries the pointer's spot along anyway; resized, the
        // edge runs away from where it was pressed and left the light behind.
        if (pressed) {
            const [pointerX, pointerY] = global.get_pointer();
            // Through the parent, and from the place just set: the card's own
            // transform is not laid out anew until the next frame.
            const [ok, parentX, parentY] = this.get_parent()?.transform_stage_point(pointerX, pointerY) ?? [false];
            const at = ok
                ? [parentX - (x - MARGIN), parentY - (y - MARGIN)]
                : [MARGIN + width / 2, MARGIN + height / 2];
            changed ||= at[0] !== this._pressAt[0] || at[1] !== this._pressAt[1];
            this._pressAt = at;
        }
        if (pressed !== this._pressed) {
            this._pressed = pressed;
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

    // Set until the frame that repaints the card is over, so each monitor it
    // spans takes the whole card.
    get forced() {
        return this._forced;
    }

    endFrame() {
        this._forced = false;
    }

    forceRepaint() {
        this._forced = true;
        this.queue_redraw();
    }

    setGlassUniforms(pipeline, scale, shareX, shareY, maxX, maxY) {
        setUniform(pipeline, 'uSize', this.width, this.height);
        setUniform(pipeline, 'uUvScale', shareX, shareY);
        setUniform(pipeline, 'uUvMax', maxX, maxY);
        setUniform(pipeline, 'uRect', ...this._rect);
        setUniform(pipeline, 'uRadius', this._radius);
        setUniform(pipeline, 'uScale', scale);
        setUniform(pipeline, 'uAppear', this._appear);
        setUniform(pipeline, 'uPress', this._press);
        setUniform(pipeline, 'uPressAt', ...this._pressAt);
        setUniform(pipeline, 'uRim', this._rim);
        setUniform(pipeline, 'uOpacity', 1);
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
        this._repaintId = 0;
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
            for (const card of this._cards.values())
                card.endFrame();
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
            // Restacking every card on every update (sixty a second while one
            // is dragged) relayouts the layer for nothing.
            if (below && card.get_previous_sibling() !== below)
                this._layer.set_child_above_sibling(card, below);
            else if (!below && this._layer.get_first_child() !== card)
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
        this._pipelines ??= makePipelines(context);
        return this._pipelines;
    }

    repaintSoon(card) {
        this._repaint.add(card);
        if (this._repaintId)
            return;
        this._repaintId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, REPAINT_DELAY_MS, () => {
            this._repaintId = 0;
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
        if (this._repaintId)
            GLib.source_remove(this._repaintId);
        this._repaintId = 0;
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

// A shell menu (the gear's settings, SYSTEM's) on the same glass as Sysi's
// cards, edge unlit like a popup's: beside a blurred top bar, the dark slab
// it had looked pasted on. The glass goes under the menu's content box, the
// part with the rounded corners, and the items paint over it.
class MenuGlass {
    constructor(menu) {
        this._menu = menu;
        this._box = menu.box;
        this._forced = false;
        this._repaintId = 0;
        this._pipelines = null;
        this._blur = null;
        this.destroyed = false;
        this._effect = new GlassEffect(this);
        this._box.add_effect(this._effect);
        menu.actor.add_style_class_name('sysi-glass-menu');
        // The box pointer paints itself offscreen, and glass there would copy
        // that empty buffer rather than the screen. Painted straight on,
        // every actor takes the menu's fading opacity itself (uOpacity).
        menu.actor.set_offscreen_redirect(0);
        this._box.connect('destroy', () => {
            this.destroyed = true;
            if (this._repaintId)
                GLib.source_remove(this._repaintId);
            this._repaintId = 0;
        });
    }

    pipelines(context) {
        this._pipelines ??= makePipelines(context);
        return this._pipelines;
    }

    blurPipelines(context, levels) {
        if (this._blur?.down.length !== levels) {
            const {down, up} = this.pipelines(context);
            this._blur = {
                down: Array.from({length: levels}, () => down.copy()),
                up: Array.from({length: levels - 1}, () => up.copy()),
            };
        }
        return this._blur;
    }

    get forced() {
        return this._forced;
    }

    setGlassUniforms(pipeline, scale, shareX, shareY, maxX, maxY) {
        // The copy has been taken by now: a forced one is done.
        this._forced = false;
        const box = this._box;
        setUniform(pipeline, 'uSize', box.width, box.height);
        setUniform(pipeline, 'uUvScale', shareX, shareY);
        setUniform(pipeline, 'uUvMax', maxX, maxY);
        setUniform(pipeline, 'uRect', 0, 0, box.width, box.height);
        setUniform(pipeline, 'uRadius', box.get_theme_node().get_border_radius(St.Corner.TOPLEFT));
        setUniform(pipeline, 'uScale', scale);
        setUniform(pipeline, 'uAppear', 1);
        setUniform(pipeline, 'uPress', 0);
        setUniform(pipeline, 'uPressAt', 0, 0);
        setUniform(pipeline, 'uRim', 0);
        setUniform(pipeline, 'uOpacity', box.get_paint_opacity() / 255);
    }

    repaintSoon() {
        if (this._repaintId)
            return;
        this._repaintId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, REPAINT_DELAY_MS, () => {
            this._repaintId = 0;
            if (!this.destroyed) {
                this._forced = true;
                this._box.queue_redraw();
            }
            return GLib.SOURCE_REMOVE;
        });
    }

    // Glass that cannot be drawn gives the menu back its plain slab.
    fail(error) {
        logError(error, 'Sysi menu glass failed');
        this._box.remove_effect(this._effect);
        this._menu.actor.remove_style_class_name('sysi-glass-menu');
    }
}

export function glassMenu(menu) {
    return new MenuGlass(menu);
}

export class GlassManager {
    enable() {
        this._hosts = new Map();
        // The last cards sent for each window, newest last.
        this._states = new Map();
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
        this._states.delete(xid);
        this._states.set(xid, {xid, width, height, cards: clean});
        // Every menu Sysi opens is a new window; keep only the recent ones.
        while (this._states.size > 16)
            this._states.delete(this._states.keys().next().value);
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
    // trusted to name one of Sysi's windows (the overlay, or a menu it has
    // opened): the window has to be Sysi's as well.
    _matches(actor, xid) {
        const window = actor.meta_window;
        if (!window || window.get_client_type() !== Meta.WindowClientType.X11)
            return false;
        if ((window.get_wm_class() ?? '').toLowerCase() !== 'sysi')
            return false;
        const [token] = (window.get_description() ?? '').split(' ');
        return token === `0x${xid.toString(16)}`;
    }

    // Hiding Sysi unmaps its window and takes the glass with it. Showing it
    // again brings a new window actor and no new rects (none changed), so the
    // last ones Sysi sent are put straight back.
    _reattach(actor) {
        if (this._failed)
            return;
        const state = [...this._states.values()].find(candidate =>
            this._matches(actor, candidate.xid));
        if (!state)
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
