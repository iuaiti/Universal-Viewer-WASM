//! Universal Viewer: look inside things without opening them.
//!
//! * **Maps** show their picture right on their slot icon, and a bigger one in the tooltip when
//!   hovered (inventory, chests, creative tabs...). No key needed.
//! * **Shulker boxes** (and any other stack that carries `minecraft:container`) and **bundles**
//!   (`minecraft:bundle_contents`) show their whole contents as a slot grid while Shift is held.
//!   Ctrl locks that grid in place, so the mouse can move over it to read each item.
//!
//! Containers and bundles are read straight from the stack's own components, so the client
//! needs nothing from the server. A map's pixels live only on the server (the client mod API has
//! no read of them), so the mod asks the server for them over a small optional payload and caches
//! the answer. On a server without this mod the map panel says so instead of drawing.

use mod_api::text::{TextContent, TextStyle};
use mod_api::{
    ClientHost, ClientTickPreEvent, DrawContext, Flow, GuiLayerPostEvent, Hooks, HostSide,
    IPayloadContext, Identifier, ItemKey, ItemPart, ItemStack, LevelDimension, PayloadCodec,
    PayloadId, Registrar, RegistrationContext, RenderTooltipGatherComponentsEvent,
    RenderTooltipPreEvent, RightClickBlockEvent, ScreenClosingEvent, ScreenKeyPressedPreEvent,
    ScreenMouseButtonPressedPreEvent, ScreenMouseButtonReleasedPreEvent,
    ScreenRenderBackgroundEvent, ScreenRenderForegroundEvent, ScreenRenderPostEvent, ScreenView,
    Tag, Text, ToClient, ToServer, TooltipComponentDrawEvent, TooltipElement, TooltipImage,
    TooltipImageSource, VarInt,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

// ---------------------------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------------------------

/// The shared mod-image slot of the HUD atlas (see `ClientHost::mod_image`).
const MOD_IMAGE_SPRITE: &str = "horizon/modimage";
/// A map is always 128 x 128 map pixels.
const MAP_SIZE: usize = 128;
/// GUI pixels one map pixel is drawn as.
const MAP_SCALE: f32 = 1.0;
/// One inventory slot, in GUI pixels.
const SLOT: f32 = 18.0;
/// Slots per row of a container grid.
const COLUMNS: usize = 9;
/// Ticks before a map that is still hovered is asked for again, so a map being explored updates.
const REFRESH_TICKS: u64 = 100;
/// Ticks before a map shown only on its slot icon is asked for again.
const ICON_REFRESH_TICKS: u64 = 400;
/// At most this many map requests go out per tick, on every other tick: ten a second, the most a
/// server running this mod answers for one player (a chest full of maps opens gradually).
const ASKS_PER_TICK: usize = 1;
/// The most map requests a server answers per player per second.
const REPLIES_PER_SECOND: u32 = 10;
/// Ticks between two uploads of the map atlas.
const UPLOAD_EVERY: u64 = 4;
/// The mod-image slot is 1024 x 1024 pixels: an 8 x 8 grid of 128 px maps.
const ATLAS: usize = 1024;
const CELLS_PER_ROW: usize = ATLAS / MAP_SIZE;
const CELLS: usize = CELLS_PER_ROW * CELLS_PER_ROW;

const TEXT_WHITE: u32 = 0xFFFF_FFFF;

// ---------------------------------------------------------------------------------------------
// Payloads: the client asks for a map, the server answers with its colour ids
// ---------------------------------------------------------------------------------------------

#[derive(Debug, PartialEq, PayloadCodec)]
struct MapRequest {
    id: VarInt,
}

#[derive(Debug, PartialEq, PayloadCodec)]
struct MapData {
    id: VarInt,
    /// Which slice of the picture this is, `0..PARTS`.
    part: VarInt,
    /// `PART_LEN` map colour ids (`base * 4 + shade`), row by row.
    colors: Vec<u8>,
    /// A map pixel is `1 << scale` blocks.
    scale: VarInt,
    /// Locked in a cartography table.
    locked: bool,
}

/// What a map's tooltip says under its picture: its zoom and whether it is locked, per id.
static INFO: Mutex<Option<HashMap<i32, (u8, bool)>>> = Mutex::new(None);

/// Records map `id`'s zoom and lock, from a vanilla map packet (the `vanilla_map` mixin) or this
/// mod's server.
pub fn vanilla_map_info(id: i32, scale: i8, locked: bool) {
    let scale = scale.clamp(0, 4) as u8;
    INFO.lock()
        .unwrap_or_else(|p| p.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(id, (scale, locked));
}

fn map_info(id: i32) -> Option<(u8, bool)> {
    INFO.lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()?
        .get(&id)
        .copied()
}

/// A payload list holds at most 4096 entries, so a 128x128 map goes in four slices.
const PART_LEN: usize = 4096;
const PARTS: usize = MAP_SIZE * MAP_SIZE / PART_LEN;

static REQUEST: Mutex<Option<ToServer<MapRequest>>> = Mutex::new(None);
static REPLY: Mutex<Option<ToClient<MapData>>> = Mutex::new(None);

// ---------------------------------------------------------------------------------------------
// Client state
// ---------------------------------------------------------------------------------------------

/// A map whose picture is in the atlas.
struct CachedMap {
    /// Its cell of the atlas grid.
    cell: usize,
    /// The atlas generation that wrote it; it shows once an upload at least that new happened.
    written: u64,
    /// When it was last drawn (slot or tooltip), to pick a cell to reuse.
    seen: u64,
    /// Whether its cell has been uploaded at least once: a refresh keeps showing the old
    /// picture until the new one is up, instead of falling back to the vanilla icon.
    shown: bool,
    /// A hash of its colour ids, so an unchanged refresh writes and uploads nothing.
    hash: u64,
}

#[derive(Default)]
struct Cache {
    /// Client ticks since the game started, a clock for refreshes and time-outs.
    tick: u64,
    maps: HashMap<i32, CachedMap>,
    /// When each map was last asked for.
    requested: HashMap<i32, u64>,
    /// The map whose tooltip was drawn since the last tick.
    hovered: Option<i32>,
    /// Maps drawn on slot icons since the last tick.
    on_slots: HashSet<i32>,
    /// Maps still arriving: their colour ids so far and which slices came.
    partial: HashMap<i32, (Vec<u8>, u8)>,
    /// The `VANILLA` version of each map last copied into the atlas.
    vanilla: HashMap<i32, u64>,
    /// All map pictures, `ATLAS * ATLAS` RGBA pixels; empty until the first map arrives.
    atlas: Vec<u8>,
    /// Bumped on every write into `atlas`.
    generation: u64,
    /// The generation last uploaded into the mod-image slot.
    uploaded: u64,
    last_upload: u64,
}

impl Cache {
    /// Whether map `id`'s picture is in the uploaded atlas, and its cell.
    fn ready(&self, id: i32) -> Option<usize> {
        let map = self.maps.get(&id)?;
        (map.shown || map.written <= self.uploaded).then_some(map.cell)
    }

    /// Writes map `id`'s pixels into its cell, taking the least recently seen cell if it is new.
    /// False when the palette is not known yet (nothing stored; try again later).
    fn store(&mut self, id: i32, colors: &[u8]) -> bool {
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            colors.hash(&mut h);
            h.finish()
        };
        let mut shown = false;
        if let Some(map) = self.maps.get_mut(&id) {
            if map.hash == hash {
                return true;
            }
            shown = map.shown || map.written <= self.uploaded;
        }
        let Some(rgba) = to_rgba(colors) else {
            return false;
        };
        if self.atlas.is_empty() {
            self.atlas = vec![0; ATLAS * ATLAS * 4];
        }
        let cell = match self.maps.get(&id) {
            Some(map) => map.cell,
            None => {
                let used: HashSet<usize> = self.maps.values().map(|m| m.cell).collect();
                match (0..CELLS).find(|cell| !used.contains(cell)) {
                    Some(free) => free,
                    None => {
                        let (&old, map) = self
                            .maps
                            .iter()
                            .min_by_key(|(_, m)| m.seen)
                            .expect("all cells are in use");
                        let cell = map.cell;
                        self.maps.remove(&old);
                        cell
                    }
                }
            }
        };
        let (cx, cy) = (cell % CELLS_PER_ROW, cell / CELLS_PER_ROW);
        let row = MAP_SIZE * 4;
        for y in 0..MAP_SIZE {
            let at = ((cy * MAP_SIZE + y) * ATLAS + cx * MAP_SIZE) * 4;
            self.atlas[at..at + row].copy_from_slice(&rgba[y * row..(y + 1) * row]);
        }
        self.generation += 1;
        let seen = self.tick;
        self.maps.insert(
            id,
            CachedMap {
                cell,
                written: self.generation,
                seen,
                shown,
                hash,
            },
        );
        true
    }
}

/// The source rectangle of atlas cell `cell`.
fn cell_rect(cell: usize) -> [f32; 4] {
    let size = MAP_SIZE as f32;
    let (cx, cy) = (cell % CELLS_PER_ROW, cell / CELLS_PER_ROW);
    [cx as f32 * size, cy as f32 * size, size, size]
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// Map pictures the server sent the game itself (vanilla's map packet, read by the
/// `vanilla_map` mixin): id to colour ids, with a counter bumped on each change. Works on any
/// server, the mod or not on it, for every map the client has been sent: one carried, held or
/// in a nearby item frame.
static VANILLA_MAPS: Mutex<Option<(u64, HashMap<i32, (u64, Vec<u8>)>)>> = Mutex::new(None);

/// Called by the `vanilla_map` mixin with a map's whole picture after a vanilla map packet.
pub fn vanilla_map_colors(id: i32, colors: Vec<u8>) {
    if colors.len() != MAP_SIZE * MAP_SIZE {
        return;
    }
    let mut guard = VANILLA_MAPS.lock().unwrap_or_else(|p| p.into_inner());
    let (counter, maps) = guard.get_or_insert_with(|| (0, HashMap::new()));
    *counter += 1;
    maps.insert(id, (*counter, colors));
}

/// Map `id`'s picture from the game's own copy, when newer than `after`.
fn vanilla_colors(id: i32, after: u64) -> Option<(u64, Vec<u8>)> {
    let guard = VANILLA_MAPS.lock().unwrap_or_else(|p| p.into_inner());
    let (_, maps) = guard.as_ref()?;
    let (version, colors) = maps.get(&id)?;
    (*version > after).then(|| (*version, colors.clone()))
}

fn with_cache<R>(f: impl FnOnce(&mut Cache) -> R) -> R {
    let mut guard = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.get_or_insert_with(Cache::default))
}

/// The game's own map colours, RGBA for each of the 256 packed colour ids (`base * 4 + shade`),
/// handed over by the `vanilla_map` mixin. Read at runtime: the mod ships no Minecraft data.
static PALETTE: std::sync::OnceLock<Vec<[u8; 4]>> = std::sync::OnceLock::new();

/// Called by the `vanilla_map` mixin with the game's colour for each packed map colour id.
pub fn vanilla_map_palette(table: &[[u8; 4]]) {
    if table.len() == 256 {
        let _ = PALETTE.set(table.to_vec());
    }
}

/// Standard Minecraft base map colours (ids 0..=61), used when no runtime palette was supplied
/// by a native mixin (e.g. WebAssembly builds).
const DEFAULT_MAP_BASE_RGB: [[u8; 3]; 62] = [
    [0, 0, 0],       // 0  NONE (transparent)
    [127, 178, 56],  // 1  GRASS
    [247, 233, 163], // 2  SAND
    [199, 199, 199], // 3  WOOL
    [255, 0, 0],     // 4  FIRE
    [160, 160, 255], // 5  ICE
    [167, 167, 167], // 6  METAL
    [0, 124, 0],     // 7  PLANT
    [255, 255, 255], // 8  SNOW
    [164, 168, 184], // 9  CLAY
    [151, 109, 77],  // 10 DIRT
    [112, 112, 112], // 11 STONE
    [64, 64, 255],   // 12 WATER
    [143, 119, 72],  // 13 WOOD
    [255, 252, 245], // 14 QUARTZ
    [216, 127, 51],  // 15 COLOR_ORANGE
    [178, 76, 216],  // 16 COLOR_MAGENTA
    [102, 153, 216], // 17 COLOR_LIGHT_BLUE
    [229, 229, 51],  // 18 COLOR_YELLOW
    [127, 204, 25],  // 19 COLOR_LIGHT_GREEN
    [242, 127, 165], // 20 COLOR_PINK
    [76, 76, 76],    // 21 COLOR_GRAY
    [153, 153, 153], // 22 COLOR_LIGHT_GRAY
    [76, 127, 153],  // 23 COLOR_CYAN
    [127, 63, 178],  // 24 COLOR_PURPLE
    [51, 76, 178],   // 25 COLOR_BLUE
    [102, 76, 51],   // 26 COLOR_BROWN
    [102, 127, 51],  // 27 COLOR_GREEN
    [153, 51, 51],   // 28 COLOR_RED
    [25, 25, 25],    // 29 COLOR_BLACK
    [250, 238, 77],  // 30 GOLD
    [92, 219, 213],  // 31 DIAMOND
    [74, 128, 255],  // 32 LAPIS
    [0, 217, 58],    // 33 EMERALD
    [129, 86, 49],   // 34 PODZOL
    [112, 2, 0],     // 35 NETHER
    [209, 177, 161], // 36 TERRACOTTA_WHITE
    [159, 82, 36],   // 37 TERRACOTTA_ORANGE
    [149, 87, 108],  // 38 TERRACOTTA_MAGENTA
    [112, 108, 138], // 39 TERRACOTTA_LIGHT_BLUE
    [186, 133, 36],  // 40 TERRACOTTA_YELLOW
    [103, 117, 53],  // 41 TERRACOTTA_LIGHT_GREEN
    [160, 77, 78],   // 42 TERRACOTTA_PINK
    [57, 41, 35],    // 43 TERRACOTTA_GRAY
    [135, 107, 98],  // 44 TERRACOTTA_LIGHT_GRAY
    [87, 92, 92],    // 45 TERRACOTTA_CYAN
    [122, 73, 88],   // 46 TERRACOTTA_PURPLE
    [76, 62, 92],    // 47 TERRACOTTA_BLUE
    [76, 50, 35],    // 48 TERRACOTTA_BROWN
    [76, 82, 42],    // 49 TERRACOTTA_GREEN
    [142, 60, 46],   // 50 TERRACOTTA_RED
    [37, 22, 16],    // 51 TERRACOTTA_BLACK
    [189, 48, 49],   // 52 CRIMSON_NYLIUM
    [148, 63, 97],   // 53 CRIMSON_STEM
    [92, 25, 29],    // 54 CRIMSON_HYPHAE
    [22, 126, 134],  // 55 WARPED_NYLIUM
    [58, 142, 140],  // 56 WARPED_STEM
    [86, 44, 62],    // 57 WARPED_HYPHAE
    [20, 180, 133],  // 58 WARPED_WART_BLOCK
    [100, 100, 100], // 59 DEEPSLATE
    [216, 175, 147], // 60 RAW_IRON
    [127, 167, 150], // 61 GLOW_LICHEN
];

const MAP_SHADE_MUL: [u16; 4] = [180, 220, 255, 135];

fn default_map_rgba(id: u8) -> [u8; 4] {
    let base = usize::from(id >> 2);
    if base == 0 || base >= DEFAULT_MAP_BASE_RGB.len() {
        return [0, 0, 0, 0];
    }
    let mul = MAP_SHADE_MUL[usize::from(id & 3)];
    let [r, g, b] = DEFAULT_MAP_BASE_RGB[base];
    [
        ((u16::from(r) * mul) / 255) as u8,
        ((u16::from(g) * mul) / 255) as u8,
        ((u16::from(b) * mul) / 255) as u8,
        255,
    ]
}

/// `128 * 128` packed colour ids to RGBA, using the runtime palette if provided or the built-in
/// standard map colour table on WebAssembly.
fn to_rgba(colors: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(colors.len() * 4);
    if let Some(palette) = PALETTE.get() {
        for &id in colors {
            out.extend_from_slice(&palette[usize::from(id)]);
        }
    } else {
        for &id in colors {
            out.extend_from_slice(&default_map_rgba(id));
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Reading a stack
// ---------------------------------------------------------------------------------------------

/// What a stack has to show.
enum View {
    Map(i32),
    Contents(Contents),
}

#[derive(Clone)]
struct Contents {
    columns: usize,
    rows: usize,
    /// Slot index and stack, in grid order.
    slots: Vec<(usize, ItemStack)>,
    /// Total items, for the footer.
    items: u64,
    /// The colour of the shulker box or bundle, `0xRRGGBB`, to tint the panel with.
    color: u32,
}

/// This mod's own tint for each dye colour, `0xRRGGBB` (picked by eye for the panels, not
/// copied from the game).
const DYES: [(&str, u32); 16] = [
    ("white", 0xF0F0EC),
    ("orange", 0xE8822A),
    ("magenta", 0xC458B8),
    ("light_blue", 0x48ACD6),
    ("yellow", 0xF2CF3E),
    ("lime", 0x84C22A),
    ("pink", 0xEB8FAE),
    ("gray", 0x4C5256),
    ("light_gray", 0x9C9C96),
    ("cyan", 0x22968F),
    ("purple", 0x8A3CB4),
    ("blue", 0x4048A8),
    ("brown", 0x80553A),
    ("green", 0x5F7C20),
    ("red", 0xAD3530),
    ("black", 0x252528),
];
/// An undyed shulker box's purple, and an undyed bundle's leather.
const PLAIN_SHULKER: u32 = 0x97_6997;
const PLAIN_BUNDLE: u32 = 0xA8_6B3C;
/// Anything else that holds items (a chest item with contents, a modded box).
const PLAIN_OTHER: u32 = 0x6E_4C9E;

/// The colour of `item` if it is `minecraft:<dye>_<kind>`, else `plain` when it is
/// `minecraft:<kind>` itself, else `other`.
fn color_of(host: &dyn ClientHost, item: ItemKey, kind: &str, plain: u32, other: u32) -> u32 {
    let is = |id: &str| {
        Identifier::parse(id)
            .ok()
            .and_then(|id| host.item_key(&id))
            .is_some_and(|key| key == item)
    };
    for (dye, rgb) in DYES {
        if is(&format!("minecraft:{dye}_{kind}")) {
            return rgb;
        }
    }
    if is(&format!("minecraft:{kind}")) {
        plain
    } else {
        other
    }
}

/// The stack's component patch as one NBT compound to look entries up in.
fn components(stack: &ItemStack) -> Option<Tag> {
    stack
        .components()
        .map(|patch| Tag::Compound(patch.as_nbt().clone()))
}

fn int(tag: &Tag) -> Option<i32> {
    match tag {
        Tag::Int(v) => Some(*v),
        Tag::Short(v) => Some(i32::from(*v)),
        Tag::Byte(v) => Some(i32::from(*v)),
        _ => None,
    }
}

fn stack_from(host: &dyn ClientHost, tag: &Tag) -> Option<ItemStack> {
    match tag {
        Tag::Compound(compound) => ItemStack::from_nbt(host, compound),
        _ => None,
    }
}

/// A map, or the contents of a container or bundle; `None` when the stack is neither.
fn view_of(host: &dyn ClientHost, stack: &ItemStack, with_contents: bool) -> Option<View> {
    let root = components(stack)?;
    if let Some(id) = root.get("minecraft:map_id").and_then(int) {
        return Some(View::Map(id));
    }
    if !with_contents {
        return None;
    }
    if let Some(Tag::List(entries)) = root.get("minecraft:container") {
        let mut slots = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let slot = entry
                .get("slot")
                .and_then(int)
                .and_then(|slot| usize::try_from(slot).ok())
                .unwrap_or(index);
            if let Some(stack) = entry.get("item").and_then(|item| stack_from(host, item)) {
                slots.push((slot, stack));
            }
        }
        if slots.is_empty() {
            return None;
        }
        let highest = slots.iter().map(|(slot, _)| *slot).max().unwrap_or(0);
        let rows = (highest / COLUMNS + 1).max(3);
        let color = color_of(
            host,
            stack.item(),
            "shulker_box",
            PLAIN_SHULKER,
            PLAIN_OTHER,
        );
        return Some(View::Contents(finish(COLUMNS, rows, slots, color)));
    }
    if let Some(Tag::List(entries)) = root.get("minecraft:bundle_contents") {
        let slots: Vec<(usize, ItemStack)> = entries
            .iter()
            .filter_map(|item| stack_from(host, item))
            .enumerate()
            .collect();
        if slots.is_empty() {
            return None;
        }
        let columns = slots.len().clamp(1, COLUMNS);
        let rows = slots.len().div_ceil(columns);
        let color = color_of(host, stack.item(), "bundle", PLAIN_BUNDLE, PLAIN_BUNDLE);
        return Some(View::Contents(finish(columns, rows, slots, color)));
    }
    None
}

fn finish(columns: usize, rows: usize, slots: Vec<(usize, ItemStack)>, color: u32) -> Contents {
    let items = slots.iter().map(|(_, s)| u64::from(s.count())).sum();
    Contents {
        columns,
        rows,
        slots,
        items,
        color,
    }
}

/// A text component as plain characters, translating keys through the selected language.
fn plain(host: &dyn ClientHost, text: &Text) -> String {
    let mut out = match &text.content {
        TextContent::Literal(literal) => literal.clone(),
        TextContent::Translatable {
            key,
            fallback,
            arguments,
        } => {
            let mut line = host
                .translate(key)
                .or_else(|| fallback.clone())
                .unwrap_or_else(|| key.clone());
            for argument in arguments {
                let value = plain(host, argument);
                if let Some(at) = line.find("%s") {
                    line.replace_range(at..at + 2, &value);
                }
            }
            line
        }
        _ => String::new(),
    };
    for child in &text.children {
        out.push_str(&plain(host, child));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------------------------

/// Gray `gray` (one channel, as vanilla's GUI textures use) multiplied by `tint` (`0xRRGGBB`), ARGB.
fn mul(gray: u32, tint: u32) -> u32 {
    let channel = |shift: u32| (((tint >> shift) & 0xFF) * gray / 255) << shift;
    0xFF00_0000 | channel(16) | channel(8) | channel(0)
}

/// A dye colour softened toward white, so even black and gray boxes keep a readable panel.
fn soften(rgb: u32) -> u32 {
    let channel = |shift: u32| {
        let c = ((rgb >> shift) & 0xFF) as f32;
        ((c * 0.7 + 255.0 * 0.3).round() as u32) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// Inner padding of the vanilla-style panel, as a container screen has.
const PAD: f32 = 7.0;

/// Where a panel of `pw` x `ph` GUI pixels goes beside the cursor, as a tooltip is placed: right
/// of it and a little up, flipped left at the screen's right edge and kept on screen.
fn place(cursor: [f32; 2], screen: [f32; 2], pw: f32, ph: f32) -> [f32; 2] {
    let [cx, cy] = cursor;
    let [sw, sh] = screen;
    let mut x = cx + 12.0;
    if x + pw + 2.0 > sw {
        x = cx - 12.0 - pw;
    }
    x = x.max(2.0);
    let mut y = cy - 12.0;
    if y + ph + 2.0 > sh {
        y = sh - ph - 2.0;
    }
    [x, y.max(2.0)]
}

/// A container-screen panel `[x, y, w, h]` (black outline, light top-left bevel, dark
/// bottom-right bevel, `#C6C6C6` face), all multiplied by `tint`.
fn vanilla_panel(draw: &mut dyn DrawContext, rect: [f32; 4], tint: u32) {
    let [x, y, pw, ph] = rect;
    let black = 0xFF00_0000;
    let (face, light, dark) = (mul(0xC6, tint), mul(0xFF, tint), mul(0x55, tint));
    // Outline with cut corners.
    draw.fill([x + 1.0, y, pw - 2.0, 1.0], black);
    draw.fill([x + 1.0, y + ph - 1.0, pw - 2.0, 1.0], black);
    draw.fill([x, y + 1.0, 1.0, ph - 2.0], black);
    draw.fill([x + pw - 1.0, y + 1.0, 1.0, ph - 2.0], black);
    draw.fill([x + 1.0, y + 1.0, pw - 2.0, ph - 2.0], face);
    // Bevels.
    draw.fill([x + 1.0, y + 1.0, pw - 4.0, 2.0], light);
    draw.fill([x + 1.0, y + 1.0, 2.0, ph - 4.0], light);
    draw.fill([x + 3.0, y + ph - 3.0, pw - 4.0, 2.0], dark);
    draw.fill([x + pw - 3.0, y + 3.0, 2.0, ph - 4.0], dark);
}

/// One sunken inventory slot at `(x, y)`, 18 x 18, tinted.
fn vanilla_slot(draw: &mut dyn DrawContext, x: f32, y: f32, tint: u32) {
    draw.fill([x, y, SLOT, SLOT], mul(0x8B, tint));
    draw.fill([x, y, SLOT - 1.0, 1.0], mul(0x37, tint));
    draw.fill([x, y, 1.0, SLOT - 1.0], mul(0x37, tint));
    draw.fill([x + 1.0, y + SLOT - 1.0, SLOT - 1.0, 1.0], mul(0xFF, tint));
    draw.fill([x + SLOT - 1.0, y + 1.0, 1.0, SLOT - 1.0], mul(0xFF, tint));
}

/// The footer's hint about the Ctrl lock.
fn lock_hint(locked: bool, toggles: bool) -> &'static str {
    match (toggles, locked) {
        (true, false) => "Ctrl: lock",
        (true, true) => "Ctrl: unlock",
        (false, false) => "Hold Ctrl: lock",
        (false, true) => "Locked",
    }
}

/// Where a contents panel's parts are, in GUI pixels.
#[derive(Clone, Copy)]
struct PanelLayout {
    /// The whole panel, `[x, y, w, h]`.
    rect: [f32; 4],
    /// The top left of the slot grid (the first slot's frame).
    grid: [f32; 2],
}

impl PanelLayout {
    fn contains(&self, point: [f32; 2]) -> bool {
        let [x, y, w, h] = self.rect;
        point[0] >= x && point[0] < x + w && point[1] >= y && point[1] < y + h
    }

    /// The grid cell (row-major index) under `point`, if any.
    fn cell_at(&self, point: [f32; 2], columns: usize, rows: usize) -> Option<usize> {
        let dx = point[0] - self.grid[0];
        let dy = point[1] - self.grid[1];
        if dx < 0.0 || dy < 0.0 {
            return None;
        }
        let (column, row) = ((dx / SLOT) as usize, (dy / SLOT) as usize);
        (column < columns && row < rows).then_some(row * columns + column)
    }
}

/// The size of a contents panel, padding included.
fn panel_size(draw: &dyn DrawContext, title: &str, contents: &Contents) -> [f32; 2] {
    let footer = format!("{} items", contents.items);
    let grid_w = contents.columns as f32 * SLOT;
    let grid_h = contents.rows as f32 * SLOT;
    let footer_w = draw.text_width(&footer) + 8.0 + draw.text_width(lock_hint(false, false));
    let w = grid_w.max(draw.text_width(title)).max(footer_w);
    let h = 11.0 + grid_h + 11.0;
    [w + PAD * 2.0, h + PAD * 2.0]
}

/// Draws a contents panel with its top left at `at`: the title, the slots (a map's picture on its
/// slot when it is here), the item count and the lock hint.
fn draw_contents_at(
    draw: &mut dyn DrawContext,
    at: [f32; 2],
    title: &str,
    contents: &Contents,
    hint: Option<&str>,
) -> PanelLayout {
    let [pw, ph] = panel_size(draw, title, contents);
    let rect = [at[0], at[1], pw, ph];
    let tint = soften(contents.color);
    vanilla_panel(draw, rect, tint);
    let (x, y, w) = (at[0] + PAD, at[1] + PAD, pw - PAD * 2.0);

    // Vanilla's dark label colour on a light face, white with a shadow on a dark one.
    let (label, shadow) = if dark_face(tint) {
        (TEXT_WHITE, true)
    } else {
        (0xFF40_4040, false)
    };
    draw.text(title, [x, y], label, shadow);

    let grid_w = contents.columns as f32 * SLOT;
    let grid_h = contents.rows as f32 * SLOT;
    let gx = x + ((w - grid_w) / 2.0).floor();
    let gy = y + 11.0;
    for row in 0..contents.rows {
        for column in 0..contents.columns {
            vanilla_slot(
                draw,
                gx + column as f32 * SLOT,
                gy + row as f32 * SLOT,
                tint,
            );
        }
    }
    for (slot, stack) in &contents.slots {
        let sx = gx + (slot % contents.columns) as f32 * SLOT;
        let sy = gy + (slot / contents.columns) as f32 * SLOT;
        let count = (stack.count() > 1).then(|| stack.count().to_string());
        draw.gui_draw_item_with(stack, [sx + 1.0, sy + 1.0], count.as_deref());
        if let Some(id) = map_id_of(stack) {
            draw_slot_map(draw, [sx + 1.0, sy + 1.0], id, stack.count());
        }
    }
    let footer = format!("{} items", contents.items);
    let fy = gy + grid_h + 3.0;
    draw.text(&footer, [x, fy], label, shadow);
    if let Some(hint) = hint {
        let hint_x = x + w - draw.text_width(hint);
        draw.text(hint, [hint_x, fy], label, shadow);
    }
    PanelLayout {
        rect,
        grid: [gx, gy],
    }
}

/// The paper a map is drawn on, under its unexplored (transparent) pixels.
const MAP_PAPER: u32 = 0xFFDD_D0B0;

/// A fixed pseudo-random number for `(a, b)`, so the torn edge never flickers.
fn noise(a: u32, b: u32) -> u32 {
    (a.wrapping_mul(73_856_093) ^ b.wrapping_mul(19_349_663)).wrapping_mul(2_654_435_761) >> 16
}

/// How deep the edge is torn at block `i` along side `side`, 0 to 2 blocks.
fn tear(side: u32, i: u32) -> u32 {
    // Mostly shallow, with some deeper bites.
    match noise(side, i / 2) % 8 {
        0 | 1 => 2,
        2 | 3 | 4 => 1,
        _ => 0,
    }
}

/// The frame's paper and the darker rim along its torn edge.
const FRAME_PAPER: u32 = 0xFFD5_BE93;
const FRAME_RIM: u32 = 0xFF8E_7D5B;

/// Vanilla's frame around a held map: 7 px of `map_background` around the 128 px picture.
const HELD_FRAME: f32 = 7.0;

/// The tooltip map size setting's key; its options, in order: 64, 96 and 128 px (the default).
const TOOLTIP_MAP_SIZE: &str = "tooltip_map_size";
const TOOLTIP_MAP_SIZES: [f32; 3] = [64.0, 96.0, 128.0];

/// The tooltip map's picture side and its frame width, the frame scaled with the picture.
fn tooltip_map_dims(host: &dyn ClientHost) -> (f32, f32) {
    let image = host
        .setting_choice(TOOLTIP_MAP_SIZE)
        .and_then(|i| TOOLTIP_MAP_SIZES.get(i as usize).copied())
        .unwrap_or(MAP_SIZE as f32 * MAP_SCALE);
    (image, HELD_FRAME * image / MAP_SIZE as f32)
}
const MAP_BACKGROUND: &str = "map/map_background";

/// Gray tooltip lines this mod adds.
const LORE_GRAY: u32 = 0xAAAA_AAFF;

/// Puts map `id`'s picture into its tooltip, under the name, as an image component the game lays
/// out with every other line (vanilla's, a server's lore and prices, other mods'), and adds its
/// zoom and lock under vanilla's id line.
fn add_map_to_tooltip(
    host: &dyn ClientHost,
    event: &mut RenderTooltipGatherComponentsEvent<'_>,
    id: i32,
) {
    let ready = with_cache(|cache| {
        cache.hovered = Some(id);
        cache.ready(id).is_some()
    });
    if !ready {
        return;
    }
    let (side, rim) = tooltip_map_dims(host);
    let outer = side + rim * 2.0;
    let image = TooltipElement::Image(TooltipImage {
        source: TooltipImageSource::ItemPart(event.stack.clone()),
        size: [outer, outer + 4.0],
    });
    let at = 1.min(event.elements.len());
    event.elements.insert(at, image);

    let Some((scale, locked)) = map_info(id) else {
        return;
    };
    let lines: Vec<String> = event
        .elements
        .iter()
        .map(|element| match element {
            TooltipElement::Text(text, _) => plain(host, text),
            TooltipElement::Image(_) => String::new(),
        })
        .collect();
    let locked_word = host
        .translate("filled_map.locked")
        .unwrap_or_else(|| "Locked".to_string());
    let id_text = format!("#{id}");
    // Under vanilla's "Id #n" (and its "Locked"), else right under the picture.
    let mut after = lines
        .iter()
        .position(|line| line.contains(&id_text))
        .map_or(at + 1, |i| i + 1);
    let has_locked = lines.iter().any(|line| line.trim() == locked_word);
    if has_locked
        && lines
            .get(after)
            .is_some_and(|line| line.trim() == locked_word)
    {
        after += 1;
    }
    let mut extra = vec![format!("Scale 1:{}", 1u32 << scale)];
    if !locked {
        extra.push("Unlocked".to_string());
    } else if !has_locked {
        extra.push(locked_word);
    }
    for (i, line) in extra.into_iter().enumerate() {
        let at = (after + i).min(event.elements.len());
        event
            .elements
            .insert(at, TooltipElement::Text(Text::literal(line), LORE_GRAY));
    }
}

/// Draws the held-map frame and the picture of the map in `stack` with its top left at `origin`.
fn draw_framed_map(
    host: &dyn ClientHost,
    draw: &mut dyn DrawContext,
    origin: [f32; 2],
    cell: usize,
) {
    let (image, rim) = tooltip_map_dims(host);
    let outer = image + rim * 2.0;
    let [fx, fy] = origin;
    // The game's own `map_background` (so a resource pack's shows too), else a drawn one.
    let frame = Identifier::parse("minecraft:map/map_background")
        .ok()
        .and_then(|sprite| host.gui_sprite(&sprite));
    let drawn = frame.is_some_and(|region| {
        let [sw, sh] = region.size;
        draw.sprite(
            MAP_BACKGROUND,
            [0.0, 0.0, sw as f32, sh as f32],
            [fx, fy, outer, outer],
            None,
        )
    });
    if !drawn {
        draw_torn_frame(draw, fx, fy, outer);
    }
    let (ix, iy) = (fx + rim, fy + rim);
    draw.sprite(
        MOD_IMAGE_SPRITE,
        cell_rect(cell),
        [ix, iy, image, image],
        None,
    );
}

/// A sheet of paper with torn edges, `size` square, for when the game has no `map_background`.
fn draw_torn_frame(draw: &mut dyn DrawContext, fx: f32, fy: f32, size: f32) {
    let blocks = (size / 2.0) as u32;
    let last = blocks - 1;
    for by in 0..blocks {
        for bx in 0..blocks {
            let sides = [
                (0, bx, by),
                (1, bx, last - by),
                (2, by, bx),
                (3, by, last - bx),
            ];
            let mut color = FRAME_PAPER;
            for (side, along, depth) in sides {
                let torn = tear(side, along);
                if depth < torn {
                    color = 0;
                    break;
                }
                if depth == torn {
                    color = FRAME_RIM;
                }
            }
            if color != 0 {
                draw.fill(
                    [fx + bx as f32 * 2.0, fy + by as f32 * 2.0, 2.0, 2.0],
                    color,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Map pictures on slots, drawn once a frame per screen (not per item)
// ---------------------------------------------------------------------------------------------
//
// An `item_decorations` part would be called for every item icon drawn, and the game converts
// each of those stacks (a full shulker box's 27 items included) to hand it over, every frame.
// So the open menu's slots are read a few times a second instead, and the maps among them drawn
// from the screen's foreground hook and the hotbar layer.

/// `minecraft:filled_map`, resolved at registration.
static FILLED_MAP: OnceLock<ItemKey> = OnceLock::new();

/// The map id of a filled map stack.
fn map_id_of(stack: &ItemStack) -> Option<i32> {
    if Some(&stack.item()) != FILLED_MAP.get() {
        return None;
    }
    components(stack)?.get("minecraft:map_id").and_then(int)
}

/// Draws map `id`'s picture over a 16 x 16 icon at `at`, then the stack count back on top, once
/// the picture is here; asks for it otherwise. True when drawn.
fn draw_slot_map(draw: &mut dyn DrawContext, at: [f32; 2], id: i32, count: u32) -> bool {
    let cell = with_cache(|cache| {
        cache.on_slots.insert(id);
        cache.ready(id)
    });
    let Some(cell) = cell else {
        return false;
    };
    let [x, y] = at;
    draw.fill([x, y, 16.0, 16.0], MAP_PAPER);
    draw.sprite(MOD_IMAGE_SPRITE, cell_rect(cell), [x, y, 16.0, 16.0], None);
    if count > 1 {
        let text = count.to_string();
        let width = draw.text_width(&text);
        draw.text(&text, [x + 17.0 - width, y + 9.0], TEXT_WHITE, true);
    }
    true
}

/// The maps in the open menu (the player's inventory when none is): slot, map id and count.
#[derive(Clone, Default)]
struct Scan {
    menu_slots: usize,
    maps: Vec<(usize, i32, u32)>,
}

/// The menu read a few slots a tick, round and round, so no tick reads the whole of a menu full
/// of shulker boxes at once.
struct ScanState {
    menu_slots: usize,
    cursor: usize,
    found: std::collections::BTreeMap<usize, (i32, u32)>,
    /// Whether the last tick saw a screen open.
    screen_open: bool,
}

static SCAN: Mutex<Option<ScanState>> = Mutex::new(None);
/// The tick to read the whole menu at (0: none pending), set by clicks and keys.
static RESCAN_AT: AtomicU64 = AtomicU64::new(0);
/// The tick a screen last drew its foreground.
static SCREEN_SEEN: AtomicU64 = AtomicU64::new(0);
/// Slots read per tick on the regular round (20 ticks a second).
const SLOTS_PER_TICK: usize = 4;

fn local_menu(host: &dyn ClientHost) -> Option<mod_api::MenuRef> {
    let player = host.local_player()?;
    player.container_menu(host as &dyn mod_api::Host)
}

/// One tick of reading: a few slots, or all of them right after a click or a screen opening.
/// With no screen open only the hotbar is read, the one place maps show then.
fn scan_step(host: &dyn ClientHost, tick: u64) {
    let Some(menu) = local_menu(host) else {
        *SCAN.lock().unwrap_or_else(|p| p.into_inner()) = None;
        return;
    };
    let read = host as &dyn mod_api::Host;
    let Some(menu_slots) = menu.slot_count(read) else {
        return;
    };
    let screen_open = tick.saturating_sub(SCREEN_SEEN.load(Ordering::Relaxed)) <= 2;
    let rescan_at = RESCAN_AT.load(Ordering::Relaxed);
    let mut whole = rescan_at != 0 && tick >= rescan_at;
    if whole {
        RESCAN_AT.store(0, Ordering::Relaxed);
    }
    let mut guard = SCAN.lock().unwrap_or_else(|p| p.into_inner());
    let state = guard.get_or_insert_with(|| ScanState {
        menu_slots: usize::MAX,
        cursor: 0,
        found: Default::default(),
        screen_open,
    });
    if state.menu_slots != menu_slots {
        state.menu_slots = menu_slots;
        state.cursor = 0;
        state.found.clear();
        whole = true;
    }
    if screen_open && !state.screen_open {
        whole = true;
    }
    state.screen_open = screen_open;
    // The player's inventory menu holds the hotbar at 36..45; any other menu ends with it.
    let hotbar_from = if menu_slots == 46 {
        36
    } else {
        menu_slots.saturating_sub(9)
    };
    let range = if screen_open {
        0..menu_slots
    } else {
        hotbar_from..(hotbar_from + 9).min(menu_slots)
    };
    let len = range.len();
    if len == 0 {
        return;
    }
    let budget = if whole { len } else { SLOTS_PER_TICK.min(len) };
    for _ in 0..budget {
        let slot = range.start + state.cursor % len;
        state.cursor = (state.cursor + 1) % len;
        match menu.slot(read, slot) {
            Some(Some(stack)) => match map_id_of(&stack) {
                Some(id) => {
                    state.found.insert(slot, (id, stack.count()));
                }
                None => {
                    state.found.remove(&slot);
                }
            },
            _ => {
                state.found.remove(&slot);
            }
        }
    }
}

/// Reads the menu again soon: now, and once more after the server's slot update.
fn rescan_soon(tick: u64) {
    RESCAN_AT.store(tick + 3, Ordering::Relaxed);
}

fn current_scan() -> Option<Scan> {
    let guard = SCAN.lock().unwrap_or_else(|p| p.into_inner());
    let state = guard.as_ref()?;
    Some(Scan {
        menu_slots: state.menu_slots,
        maps: state
            .found
            .iter()
            .map(|(slot, (id, count))| (*slot, *id, *count))
            .collect(),
    })
}

// ---------------------------------------------------------------------------------------------
// Ctrl lock: a contents panel that stays put while the mouse reads it
// ---------------------------------------------------------------------------------------------

/// A locked contents panel.
struct Locked {
    title: String,
    contents: Contents,
    at: [f32; 2],
    /// The screen it was locked on; another screen unlocks it.
    screen: String,
    menu: Option<String>,
    /// Where it was last drawn.
    layout: Option<PanelLayout>,
}

/// The contents panel drawn last, which Ctrl locks.
struct Candidate {
    tick: u64,
    title: String,
    contents: Contents,
    at: [f32; 2],
}

static LOCK: Mutex<Option<Locked>> = Mutex::new(None);
static CANDIDATE: Mutex<Option<Candidate>> = Mutex::new(None);
/// A mouse press over the locked panel was eaten, so its release is too.
static PRESS_EATEN: AtomicBool = AtomicBool::new(false);

/// The locked panel's rectangle, when one is locked and drawn.
fn locked_rect() -> Option<PanelLayout> {
    LOCK.lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()?
        .layout
}

fn unlock() {
    *LOCK.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// The lock setting's key; its options, in order: hold Ctrl (the default), or press it to lock
/// and again to unlock.
const LOCK_MODE: &str = "lock_mode";

/// Whether Ctrl toggles the lock (else the lock lasts while Ctrl is held).
fn lock_toggles(host: &dyn ClientHost) -> bool {
    host.setting_choice(LOCK_MODE) == Some(1)
}

fn is_control(key: &str) -> bool {
    matches!(
        key,
        "key.keyboard.left.control" | "key.keyboard.right.control"
    )
}

/// A text component stored as NBT (a string, or a compound as JSON describes one) as `Text`.
fn text_from_tag(host: &dyn ClientHost, tag: &Tag) -> Option<Text> {
    match tag {
        Tag::String(text) => Some(Text::literal(text.to_string_lossy())),
        Tag::Compound(_) | Tag::List(_) => host.text_from_json(&tag_json(tag)).ok().flatten(),
        _ => None,
    }
}

/// An NBT tag written as JSON: what a text component's NBT form says, in its JSON form.
fn tag_json(tag: &Tag) -> String {
    fn quote(text: &str, out: &mut String) {
        out.push('"');
        for c in text.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    fn write(tag: &Tag, out: &mut String) {
        match tag {
            Tag::String(text) => quote(&text.to_string_lossy(), out),
            // Text components keep their flags (bold, italic...) as bytes.
            Tag::Byte(v) => out.push_str(if *v != 0 { "true" } else { "false" }),
            Tag::Short(v) => out.push_str(&v.to_string()),
            Tag::Int(v) => out.push_str(&v.to_string()),
            Tag::Long(v) => out.push_str(&v.to_string()),
            Tag::Float(bits) => out.push_str(&f32::from_bits(*bits).to_string()),
            Tag::Double(bits) => out.push_str(&f64::from_bits(*bits).to_string()),
            Tag::Compound(map) => {
                out.push('{');
                for (i, (key, value)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    quote(&key.to_string_lossy(), out);
                    out.push(':');
                    write(value, out);
                }
                out.push('}');
            }
            Tag::List(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            Tag::IntArray(items) => {
                let list: Vec<String> = items.iter().map(i32::to_string).collect();
                out.push_str(&format!("[{}]", list.join(",")));
            }
            _ => out.push_str("null"),
        }
    }
    let mut out = String::new();
    write(tag, &mut out);
    out
}

/// `text` under a parent of colour `color` (a name or `#RRGGBB`), italic when asked: its own style
/// wins where it sets one.
fn styled(text: Text, color: &str, italic: bool) -> Text {
    let mut style = TextStyle::default();
    style.color = Some(color.to_string());
    if italic {
        style.italic = Some(true);
    }
    Text::literal("").with_style(style).append(text)
}

/// The tooltip lines of an item inside a locked panel: its name, enchantments, map id, lore, and
/// the Shift hint for something that holds items.
fn item_lines(host: &dyn ClientHost, stack: &ItemStack) -> Vec<Text> {
    let root = components(stack).unwrap_or(Tag::Compound(Default::default()));
    let mut lines = Vec::new();
    let name = root
        .get("minecraft:custom_name")
        .and_then(|tag| text_from_tag(host, tag))
        .map(|name| styled(name, "white", true))
        .unwrap_or_else(|| Text::literal(host.hover_name(stack)));
    lines.push(name);
    for key in ["minecraft:enchantments", "minecraft:stored_enchantments"] {
        let Some(enchantments) = root.get(key) else {
            continue;
        };
        let levels = enchantments.get("levels").unwrap_or(enchantments);
        let Tag::Compound(levels) = levels else {
            continue;
        };
        for (id, level) in levels {
            let id = id.to_string_lossy();
            let Some(level) = int(level) else {
                continue;
            };
            let (namespace, path) = id.split_once(':').unwrap_or(("minecraft", id.as_str()));
            let name = host
                .translate(&format!("enchantment.{namespace}.{path}"))
                .unwrap_or_else(|| path.replace('_', " "));
            let line = if level > 1 {
                let roman = host
                    .translate(&format!("enchantment.level.{level}"))
                    .unwrap_or_else(|| level.to_string());
                format!("{name} {roman}")
            } else {
                name
            };
            let color = if path.contains("curse") {
                "red"
            } else {
                "gray"
            };
            lines.push(styled(Text::literal(line), color, false));
        }
    }
    if let Some(id) = root.get("minecraft:map_id").and_then(int) {
        lines.push(styled(Text::literal(format!("Map #{id}")), "gray", false));
        // A server sends a map's picture only for a map the player carries or sees framed.
        lines.push(styled(
            Text::literal("Picture not loaded: carry this map once"),
            "dark_gray",
            true,
        ));
    }
    if let Some(Tag::List(lore)) = root.get("minecraft:lore") {
        for line in lore {
            if let Some(line) = text_from_tag(host, line) {
                lines.push(styled(line, "dark_purple", true));
            }
        }
    }
    if matches!(view_of(host, stack, true), Some(View::Contents(_))) {
        lines.push(styled(
            Text::literal("Hold Shift to view contents"),
            "#FF5555",
            false,
        ));
    }
    lines
}

/// Vanilla's tooltip colours, for a map shown from a locked panel.
const TIP_BACKGROUND: u32 = 0xF010_0010;
const TIP_TOP: u32 = 0xF050_00FF;
const TIP_BOTTOM: u32 = 0xF028_007F;

/// A vanilla tooltip box around content `[x, y, w, h]`.
fn tooltip_box(draw: &mut dyn DrawContext, rect: [f32; 4]) {
    let [x, y, w, h] = rect;
    draw.fill([x - 3.0, y - 4.0, w + 6.0, h + 8.0], TIP_BACKGROUND);
    draw.fill([x - 3.0, y - 4.0, w + 6.0, 1.0], TIP_TOP);
    draw.fill([x - 3.0, y + h + 3.0, w + 6.0, 1.0], TIP_BOTTOM);
    draw.fill_gradient([x - 3.0, y - 3.0, 1.0, h + 6.0], TIP_TOP, TIP_BOTTOM);
    draw.fill_gradient([x + w + 2.0, y - 3.0, 1.0, h + 6.0], TIP_TOP, TIP_BOTTOM);
}

/// What hovering `stack` in a locked panel shows at `mouse`: its own contents panel (with Shift),
/// its map picture, or its tooltip.
fn show_inner(
    host: &dyn ClientHost,
    draw: &mut dyn DrawContext,
    stack: &ItemStack,
    mouse: [f32; 2],
) {
    let screen = draw.size();
    let name = host.hover_name(stack);
    if host.modifiers().shift()
        && let Some(View::Contents(contents)) = view_of(host, stack, true)
    {
        let [pw, ph] = panel_size(draw, &name, &contents);
        let at = place(mouse, screen, pw, ph);
        draw_contents_at(draw, at, &name, &contents, None);
        return;
    }
    if let Some(id) = map_id_of(stack) {
        let cell = with_cache(|cache| {
            cache.hovered = Some(id);
            cache.ready(id)
        });
        if let Some(cell) = cell {
            let (side, rim) = tooltip_map_dims(host);
            let outer = side + rim * 2.0;
            let mut lines = vec![format!("Map #{id}")];
            if let Some((scale, locked)) = map_info(id) {
                lines.push(format!("Scale 1:{}", 1u32 << scale));
                lines.push(if locked { "Locked" } else { "Unlocked" }.to_string());
            }
            let w = lines
                .iter()
                .map(|line| draw.text_width(line))
                .fold(outer.max(draw.text_width(&name)), f32::max);
            let h = 12.0 + outer + 4.0 + lines.len() as f32 * 10.0;
            let [x, y] = place(mouse, screen, w + 8.0, h + 8.0);
            let (x, y) = (x + 4.0, y + 4.0);
            tooltip_box(draw, [x, y, w, h]);
            draw.text(&name, [x, y], TEXT_WHITE, true);
            draw_framed_map(host, draw, [x, y + 12.0], cell);
            for (i, line) in lines.iter().enumerate() {
                let ly = y + 12.0 + outer + 4.0 + i as f32 * 10.0;
                draw.text(line, [x, ly], 0xFFAA_AAAA, true);
            }
            return;
        }
    }
    let _ = draw.gui_tooltip(&item_lines(host, stack), mouse);
}

// ---------------------------------------------------------------------------------------------
// A placed shulker box's own screen, tinted like the box
// ---------------------------------------------------------------------------------------------

/// The colour of the shulker box the player last right-clicked, `0xRRGGBB`; None after a click
/// on anything else.
static OPENED: Mutex<Option<u32>> = Mutex::new(None);

/// The vanilla shulker box screen: 176 x 166, 3 x 9 box slots, then the inventory and hotbar.
const SCREEN_W: f32 = 176.0;
const SCREEN_H: f32 = 166.0;
/// Top-left of each 9-wide slot row block (the slot frame, one pixel outside the item), rows.
const SCREEN_GRIDS: [(f32, f32, usize); 3] = [(7.0, 17.0, 3), (7.0, 83.0, 3), (7.0, 141.0, 1)];

/// The colour of a shulker box block or item named `id` (`minecraft:red_shulker_box`), or None
/// for anything else.
fn shulker_color_by_name(id: &str) -> Option<u32> {
    let path = id.split('[').next()?.rsplit(':').next()?;
    if path == "shulker_box" {
        return Some(PLAIN_SHULKER);
    }
    let dye = path.strip_suffix("_shulker_box")?;
    DYES.iter()
        .find(|(name, _)| *name == dye)
        .map(|(_, rgb)| *rgb)
}

/// The colour of the shulker box under the crosshair, the block whose screen is open.
fn aimed_shulker(host: &dyn ClientHost) -> Option<u32> {
    match host.crosshair_target()? {
        mod_api::CrosshairTarget::Block { state, .. } => {
            shulker_color_by_name(&host.block_state_string(state)?)
        }
        _ => None,
    }
}

/// The open placed shulker box's panel (its top left) and tint: the screen of a shulker box menu,
/// or of a 3-row chest-sized menu while the crosshair is on a shulker box.
fn shulker_screen(host: &dyn ClientHost, screen: &ScreenView<'_>) -> Option<([f32; 2], u32)> {
    let color =
        aimed_shulker(host).or_else(|| *OPENED.lock().unwrap_or_else(|p| p.into_inner()))?;
    let shulker_menu = screen.menu == Some("minecraft:shulker_box");
    if !shulker_menu && screen.slots != 63 {
        return None;
    }
    let [x, y, w, h] = screen.rect;
    let at = if (w - SCREEN_W).abs() <= 0.5 && (h - SCREEN_H).abs() <= 0.5 {
        [x, y]
    } else {
        // The rectangle is the whole window: the panel sits in its middle, as vanilla centres it.
        [
            x + ((w - SCREEN_W) / 2.0).floor(),
            y + ((h - SCREEN_H) / 2.0).floor(),
        ]
    };
    Some((at, soften(color)))
}

/// Whether labels on a face of this tint want white text.
fn dark_face(tint: u32) -> bool {
    let face = mul(0xC6, tint);
    let luma = ((face >> 16) & 0xFF) * 3 + ((face >> 8) & 0xFF) * 6 + (face & 0xFF);
    luma <= 1300
}

// ---------------------------------------------------------------------------------------------
// The mod
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct UniversalViewer;

#[mod_api::horizon_mod]
impl Hooks for UniversalViewer {
    fn register(&self, r: &mut dyn Registrar, _: &RegistrationContext) {
        let mut events = r.payload_handlers();
        // Optional: the mod still loads, and joins, a server that doesn't have it.
        let mut payloads = events.registrar("1").optional();

        let reply = payloads.play_to_client_typed::<MapData>(
            PayloadId("universal_viewer:map_data".into()),
            on_map_data,
        );
        *REPLY.lock().unwrap_or_else(|p| p.into_inner()) = Some(reply);

        let request = payloads.play_to_server_typed::<MapRequest>(
            PayloadId("universal_viewer:map_request".into()),
            on_map_request,
        );
        *REQUEST.lock().unwrap_or_else(|p| p.into_inner()) = Some(request);

        // Options > Mods > Universal Viewer: how Ctrl locks a contents panel.
        let _ = r.register_setting(
            mod_api::SettingSpec::choice(
                LOCK_MODE,
                "universal_viewer.setting.lock_mode",
                [
                    "universal_viewer.setting.lock_mode.hold",
                    "universal_viewer.setting.lock_mode.toggle",
                ],
                0,
            )
            .tooltip("universal_viewer.setting.lock_mode.tooltip"),
        );
        // How large a map's picture shows in its tooltip.
        let _ = r.register_setting(
            mod_api::SettingSpec::choice(
                TOOLTIP_MAP_SIZE,
                "universal_viewer.setting.tooltip_map_size",
                [
                    "universal_viewer.setting.tooltip_map_size.64",
                    "universal_viewer.setting.tooltip_map_size.96",
                    "universal_viewer.setting.tooltip_map_size.128",
                ],
                2,
            )
            .tooltip("universal_viewer.setting.tooltip_map_size.tooltip"),
        );

        // The map picture inside a map's tooltip.
        let map = r.item_key("minecraft:filled_map");
        let _ = FILLED_MAP.set(map);
        r.item_part(map, MapIcon);
    }

    /// Once a tick: ask the server for the maps on screen, and upload the atlas when it changed.
    fn client_tick_pre(&self, host: &mut dyn ClientHost, _: &mut ClientTickPreEvent) {
        let tick = with_cache(|cache| {
            cache.tick += 1;
            cache.tick
        });
        // The open menu's maps: a few slots each tick, all of them after a click.
        scan_step(&*host, tick);
        let asks = with_cache(|cache| {
            let hovered = cache.hovered.take();
            let on_slots = std::mem::take(&mut cache.on_slots);
            let mut asks = Vec::new();
            // The hovered map first, refreshed more often, then the slot icons.
            let wanted = hovered
                .iter()
                .map(|id| (*id, REFRESH_TICKS))
                .chain(on_slots.iter().map(|id| (*id, ICON_REFRESH_TICKS)));
            for (id, refresh) in wanted {
                if let Some(map) = cache.maps.get_mut(&id) {
                    map.seen = tick;
                }
                // The game's own copy first: no server help needed.
                let after = if cache.maps.contains_key(&id) {
                    cache.vanilla.get(&id).copied().unwrap_or(0)
                } else {
                    0
                };
                if let Some((version, colors)) = vanilla_colors(id, after)
                    && cache.store(id, &colors)
                {
                    cache.vanilla.insert(id, version);
                }
                if cache.vanilla.contains_key(&id) && cache.maps.contains_key(&id) {
                    continue;
                }
                if tick % 2 == 1 || asks.len() >= ASKS_PER_TICK || asks.contains(&id) {
                    continue;
                }
                let have = cache.maps.contains_key(&id);
                let wait = if have { refresh } else { REFRESH_TICKS };
                let due = cache
                    .requested
                    .get(&id)
                    .is_none_or(|at| tick.saturating_sub(*at) > wait);
                if due {
                    cache.requested.insert(id, tick);
                    asks.push(id);
                }
            }
            if cache.generation > cache.uploaded
                && tick.saturating_sub(cache.last_upload) >= UPLOAD_EVERY
                && host.mod_image(ATLAS as u32, ATLAS as u32, &cache.atlas)
            {
                cache.uploaded = cache.generation;
                cache.last_upload = tick;
                for map in cache.maps.values_mut() {
                    map.shown = true;
                }
            }
            asks
        });
        if asks.is_empty() {
            return;
        }
        let token = REQUEST.lock().unwrap_or_else(|p| p.into_inner()).clone();
        if let Some(token) = token {
            for id in asks {
                // An unsupported peer (a server without the mod) just never answers.
                let _ = token.send(host, &MapRequest { id: VarInt(id) });
            }
        }
    }

    /// Remembers the colour of a shulker box the player right-clicks, for its screen.
    fn right_click_block(&self, host: HostSide, event: &mut RightClickBlockEvent<'_>) -> Flow {
        let HostSide::Client(host) = host else {
            return Flow::Continue;
        };
        let dimension = match event.click.dimension {
            LevelDimension::Overworld => "minecraft:overworld",
            LevelDimension::Nether => "minecraft:the_nether",
            LevelDimension::End => "minecraft:the_end",
            _ => {
                *OPENED.lock().unwrap_or_else(|p| p.into_inner()) = None;
                return Flow::Continue;
            }
        };
        let color = host
            .level(dimension)
            .and_then(|level| level.block(&*host as &dyn mod_api::Host, event.click.pos))
            .and_then(|block| host.block_item(block))
            .map(|item| color_of(&*host, item, "shulker_box", PLAIN_SHULKER, 0))
            .filter(|color| *color != 0);
        *OPENED.lock().unwrap_or_else(|p| p.into_inner()) = color;
        Flow::Continue
    }

    /// Repaints a placed shulker box's screen panel and slots in the box's colour, under its
    /// items and labels.
    fn screen_render_background(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenRenderBackgroundEvent<'_>,
    ) {
        let Some(([x, y], tint)) = shulker_screen(host, event.screen) else {
            return;
        };
        let draw = &mut *event.draw;
        let black = 0xFF00_0000;
        let (face, light, dark) = (mul(0xC6, tint), mul(0xFF, tint), mul(0x55, tint));
        let (w, h) = (SCREEN_W, SCREEN_H);
        draw.fill([x + 1.0, y, w - 2.0, 1.0], black);
        draw.fill([x + 1.0, y + h - 1.0, w - 2.0, 1.0], black);
        draw.fill([x, y + 1.0, 1.0, h - 2.0], black);
        draw.fill([x + w - 1.0, y + 1.0, 1.0, h - 2.0], black);
        draw.fill([x + 1.0, y + 1.0, w - 2.0, h - 2.0], face);
        draw.fill([x + 1.0, y + 1.0, w - 4.0, 2.0], light);
        draw.fill([x + 1.0, y + 1.0, 2.0, h - 4.0], light);
        draw.fill([x + 3.0, y + h - 3.0, w - 4.0, 2.0], dark);
        draw.fill([x + w - 3.0, y + 3.0, 2.0, h - 4.0], dark);
        // The slots where the screen really has them (its slot boxes are the 16 x 16 item
        // squares, the frame one pixel around), else vanilla's layout.
        let boxes = local_menu(host).and_then(|menu| menu.slot_boxes(host));
        match boxes {
            Some(boxes) if !boxes.is_empty() => {
                for [sx, sy, _, _] in boxes.into_iter().flatten() {
                    vanilla_slot(draw, sx - 1.0, sy - 1.0, tint);
                }
            }
            _ => {
                for (gx, gy, rows) in SCREEN_GRIDS {
                    for row in 0..rows {
                        for column in 0..COLUMNS {
                            let sx = x + gx + column as f32 * SLOT;
                            let sy = y + gy + row as f32 * SLOT;
                            vanilla_slot(draw, sx, sy, tint);
                        }
                    }
                }
            }
        }
    }

    /// On a dark box, writes the screen's labels again in white so they stay readable.
    fn screen_render_foreground(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenRenderForegroundEvent<'_>,
    ) {
        SCREEN_SEEN.store(with_cache(|cache| cache.tick), Ordering::Relaxed);
        draw_menu_maps(host, &mut *event.draw, event.screen.mouse);
        let Some(([x, y], tint)) = shulker_screen(host, event.screen) else {
            return;
        };
        if !dark_face(tint) {
            return;
        }
        let title = event
            .screen
            .title
            .map(|t| plain(host, t))
            .unwrap_or_default();
        let inventory = host
            .translate("container.inventory")
            .unwrap_or_else(|| "Inventory".to_string());
        event
            .draw
            .text(&title, [x + 8.0, y + 6.0], TEXT_WHITE, true);
        event
            .draw
            .text(&inventory, [x + 8.0, y + 72.0], TEXT_WHITE, true);
    }

    /// A hint on things that hold items, so the Shift trick is discoverable.
    fn render_tooltip_gather_components(
        &self,
        host: &dyn ClientHost,
        event: &mut RenderTooltipGatherComponentsEvent<'_>,
    ) -> Flow {
        if !host.modifiers().shift()
            && matches!(view_of(host, event.stack, true), Some(View::Contents(_)))
        {
            event.elements.push(TooltipElement::Text(
                Text::literal("Hold Shift to view contents"),
                // Bright red (`rgba`).
                0xFF55_55FF,
            ));
        }
        if let Some(View::Map(id)) = view_of(host, event.stack, false) {
            add_map_to_tooltip(host, event, id);
        }
        Flow::Continue
    }

    /// Takes the hover over for a map (always) or a container/bundle (with Shift held).
    fn render_tooltip_pre(
        &self,
        host: &dyn ClientHost,
        event: &mut RenderTooltipPreEvent<'_>,
    ) -> Flow {
        // Under a locked panel the panel answers the hover (`screen_render_post`).
        let cursor = [event.place.x as f32, event.place.y as f32];
        if locked_rect().is_some_and(|layout| layout.contains(cursor)) {
            return Flow::Cancel;
        }
        let Some(stack) = event.stack else {
            return Flow::Continue;
        };
        let modifiers = host.modifiers();
        let shift = modifiers.shift();
        let Some(view) = view_of(host, stack, shift) else {
            return Flow::Continue;
        };
        let title = event
            .elements
            .iter()
            .find_map(|element| match element {
                TooltipElement::Text(text, _) => Some(plain(host, text)),
                TooltipElement::Image(_) => None,
            })
            .unwrap_or_default();
        let screen = [event.screen_width as f32, event.screen_height as f32];
        let draw = &mut *event.draw;

        match view {
            View::Contents(contents) => {
                let [pw, ph] = panel_size(draw, &title, &contents);
                let at = place(cursor, screen, pw, ph);
                draw_contents_at(
                    draw,
                    at,
                    &title,
                    &contents,
                    Some(lock_hint(false, lock_toggles(host))),
                );
                let tick = with_cache(|cache| cache.tick);
                *CANDIDATE.lock().unwrap_or_else(|p| p.into_inner()) = Some(Candidate {
                    tick,
                    title,
                    contents,
                    at,
                });
            }
            View::Map(id) => {
                // The picture is a component of the game's own tooltip (`add_map_to_tooltip`).
                with_cache(|cache| cache.hovered = Some(id));
                return Flow::Continue;
            }
        }
        Flow::Cancel
    }

    /// Maps on the hotbar, drawn over its items.
    fn gui_layer_post(&self, _: &dyn ClientHost, event: &mut GuiLayerPostEvent<'_>) {
        if event.layer != "minecraft:hotbar" {
            return;
        }
        let Some(scan) = current_scan() else {
            return;
        };
        // The player's inventory menu holds the hotbar at 36..45; any other menu ends with it.
        let first = match scan.menu_slots {
            46 => 36,
            n if n >= 9 => n - 9,
            _ => return,
        };
        let draw = &mut *event.draw;
        let [w, h] = draw.size();
        let left = (w / 2.0).floor() - 88.0;
        for (slot, id, count) in scan.maps {
            if (first..first + 9).contains(&slot) {
                let at = [left + (slot - first) as f32 * 20.0, h - 19.0];
                draw_slot_map(draw, at, id, count);
            }
        }
    }

    /// Ctrl over a contents panel locks it in place (or unlocks it).
    fn screen_key_pressed_pre(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenKeyPressedPreEvent<'_>,
    ) -> Flow {
        let tick = with_cache(|cache| cache.tick);
        if !is_control(event.key) {
            // A key can move items (a hotbar number, Q): look at the slots again soon.
            rescan_soon(tick);
            return Flow::Continue;
        }
        if event.repeat {
            return Flow::Continue;
        }
        let mut lock = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        if lock.is_some() {
            if lock_toggles(host) {
                *lock = None;
            }
            return Flow::Continue;
        }
        let candidate = CANDIDATE.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(candidate) = candidate
            && tick.saturating_sub(candidate.tick) <= 2
        {
            *lock = Some(Locked {
                title: candidate.title,
                contents: candidate.contents,
                at: candidate.at,
                screen: event.screen.id.to_string(),
                menu: event.screen.menu.map(str::to_string),
                layout: None,
            });
        }
        Flow::Continue
    }

    /// A click on the locked panel stays on it, so it never takes the slot underneath.
    fn screen_mouse_button_pressed_pre(
        &self,
        _: &dyn ClientHost,
        event: &mut ScreenMouseButtonPressedPreEvent<'_>,
    ) -> Flow {
        if locked_rect().is_some_and(|layout| layout.contains(event.screen.mouse)) {
            PRESS_EATEN.store(true, Ordering::Relaxed);
            return Flow::Cancel;
        }
        Flow::Continue
    }

    fn screen_mouse_button_released_pre(
        &self,
        _: &dyn ClientHost,
        _: &mut ScreenMouseButtonReleasedPreEvent<'_>,
    ) -> Flow {
        if PRESS_EATEN.swap(false, Ordering::Relaxed) {
            return Flow::Cancel;
        }
        // A click may have moved a map: look at the slots again soon.
        rescan_soon(with_cache(|cache| cache.tick));
        Flow::Continue
    }

    /// The locked panel, over everything, and what the mouse points at in it.
    fn screen_render_post(&self, host: &dyn ClientHost, event: &mut ScreenRenderPostEvent<'_>) {
        let mut guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let Some(locked) = guard.as_mut() else {
            return;
        };
        if event.screen.id != locked.screen || event.screen.menu != locked.menu.as_deref() {
            *guard = None;
            return;
        }
        // Hold mode: letting go of Ctrl lets go of the panel.
        if !lock_toggles(host) && !host.modifiers().control() {
            *guard = None;
            return;
        }
        let draw = &mut *event.draw;
        let layout = draw_contents_at(
            draw,
            locked.at,
            &locked.title,
            &locked.contents,
            Some(lock_hint(true, lock_toggles(host))),
        );
        locked.layout = Some(layout);
        let mouse = event.screen.mouse;
        let contents = &locked.contents;
        let Some(cell) = layout.cell_at(mouse, contents.columns, contents.rows) else {
            return;
        };
        let Some((_, stack)) = contents.slots.iter().find(|(slot, _)| *slot == cell) else {
            return;
        };
        let sx = layout.grid[0] + (cell % contents.columns) as f32 * SLOT;
        let sy = layout.grid[1] + (cell / contents.columns) as f32 * SLOT;
        draw.fill([sx + 1.0, sy + 1.0, 16.0, 16.0], 0x80FF_FFFF);
        let stack = stack.clone();
        drop(guard);
        show_inner(host, draw, &stack, mouse);
    }

    /// Leaving the screen unlocks its panel.
    fn screen_closing(&self, _: &dyn ClientHost, _: &mut ScreenClosingEvent<'_>) {
        unlock();
        *CANDIDATE.lock().unwrap_or_else(|p| p.into_inner()) = None;
        rescan_soon(with_cache(|cache| cache.tick));
    }
}

/// Draws the maps of the open menu over their slots, with the hover highlight back on top.
fn draw_menu_maps(host: &dyn ClientHost, draw: &mut dyn DrawContext, mouse: [f32; 2]) {
    let Some(scan) = current_scan() else {
        return;
    };
    if scan.maps.is_empty() {
        return;
    }
    let Some(boxes) = local_menu(host).and_then(|menu| menu.slot_boxes(host)) else {
        return;
    };
    if boxes.len() != scan.menu_slots {
        // Another menu than the one read; the next tick reads this one.
        return;
    }
    for (slot, id, count) in scan.maps {
        let Some(Some([x, y, w, h])) = boxes.get(slot).copied() else {
            continue;
        };
        if draw_slot_map(draw, [x, y], id, count)
            && mouse[0] >= x
            && mouse[0] < x + w
            && mouse[1] >= y
            && mouse[1] < y + h
        {
            draw.fill([x, y, 16.0, 16.0], 0x80FF_FFFF);
        }
    }
}

/// The map picture component of a map's tooltip.
#[derive(Default)]
pub struct MapIcon;

#[mod_api::horizon_part]
impl ItemPart for MapIcon {
    /// The map picture component `add_map_to_tooltip` put in the tooltip.
    fn tooltip_component_draw(
        &self,
        host: &dyn ClientHost,
        event: &mut TooltipComponentDrawEvent<'_>,
    ) {
        let Some(View::Map(id)) = view_of(host, event.stack, false) else {
            return;
        };
        let Some(cell) = with_cache(|cache| cache.ready(id)) else {
            return;
        };
        draw_framed_map(host, &mut *event.draw, event.origin, cell);
    }
}

// ---------------------------------------------------------------------------------------------
// Payload handlers
// ---------------------------------------------------------------------------------------------

/// Server: a client wants map `id`. Answer with its colours, in slices, when the world has it.
/// Never returns an error: a failed handler disconnects the player.
fn on_map_request(request: MapRequest, cx: &mut dyn IPayloadContext) -> Result<(), String> {
    let id = request.id.0;
    let Ok(player) = cx.player() else {
        return Ok(());
    };
    // Bounded work: a player gets at most `REPLIES_PER_SECOND` answers a second.
    if !allow_reply(player) {
        return Ok(());
    }
    let state = {
        let Ok(host) = cx.server_host() else {
            return Ok(());
        };
        // Only a map the player can see: one they carry or that the menu they have open
        // shows, directly or inside a shulker box or bundle there (what the Shift preview
        // shows them).
        if !player_sees_map(&*host, player, id) {
            return Ok(());
        }
        // Maps belong to the world, so any level reaches them.
        let Some(level) = host.level("minecraft:overworld") else {
            return Ok(());
        };
        level.map_data(host, id)
    };
    let Some(state) = state else {
        return Ok(());
    };
    let (scale, locked, colors) = (state.scale, state.locked, state.colors);
    if colors.len() != MAP_SIZE * MAP_SIZE {
        return Ok(());
    }
    let Some(token) = REPLY.lock().unwrap_or_else(|p| p.into_inner()).clone() else {
        return Ok(());
    };
    for (part, slice) in colors.chunks(PART_LEN).enumerate() {
        let data = MapData {
            id: VarInt(id),
            part: VarInt(part as i32),
            colors: slice.to_vec(),
            scale: VarInt(i32::from(scale)),
            locked,
        };
        if token.reply(cx, &data).is_err() {
            break;
        }
    }
    Ok(())
}

/// Requests answered per player in the current second (native only; `std::time::Instant` is
/// unavailable on `wasm32-unknown-unknown`).
#[cfg(not(target_arch = "wasm32"))]
static REPLY_BUDGET: Mutex<Option<HashMap<mod_api::PlayerRef, (std::time::Instant, u32)>>> =
    Mutex::new(None);

/// Whether `player` may have one more map answered this second.
#[cfg(not(target_arch = "wasm32"))]
fn allow_reply(player: mod_api::PlayerRef) -> bool {
    let now = std::time::Instant::now();
    let mut guard = REPLY_BUDGET.lock().unwrap_or_else(|p| p.into_inner());
    let budget = guard.get_or_insert_with(HashMap::new);
    // Forget players idle for a while, so the table stays as small as the player list.
    budget.retain(|_, (since, _)| now.duration_since(*since).as_secs() < 10);
    let (since, used) = budget.entry(player).or_insert((now, 0));
    if now.duration_since(*since).as_secs_f32() >= 1.0 {
        *since = now;
        *used = 0;
    }
    if *used >= REPLIES_PER_SECOND {
        return false;
    }
    *used += 1;
    true
}

#[cfg(target_arch = "wasm32")]
fn allow_reply(_player: mod_api::PlayerRef) -> bool {
    let _ = REPLIES_PER_SECOND;
    true
}

/// Adds the map ids an item shows to `out`: its own, and those of the items it holds (a shulker
/// box's or a bundle's), two levels deep. `root` is the item's components.
fn collect_map_ids(root: &Tag, out: &mut HashSet<i32>, depth: u32) {
    if let Some(id) = root.get("minecraft:map_id").and_then(int) {
        out.insert(id);
    }
    if depth >= 2 {
        return;
    }
    for key in ["minecraft:container", "minecraft:bundle_contents"] {
        let Some(Tag::List(entries)) = root.get(key) else {
            continue;
        };
        for entry in entries {
            // A container entry is `{slot, item}`; a bundle's is the item itself.
            let item = entry.get("item").unwrap_or(entry);
            if let Some(components) = item.get("components") {
                collect_map_ids(components, out, depth + 1);
            }
        }
    }
}

/// Server: whether map `id` is in what `player` can see: their inventory, or the slots of the
/// menu they have open (a chest's, a shulker box's, their ender chest's), held items included.
fn player_sees_map(host: &dyn mod_api::ServerHost, player: mod_api::PlayerRef, id: i32) -> bool {
    let mut seen = HashSet::new();
    let mut look = |stack: &ItemStack| {
        if let Some(root) = components(stack) {
            collect_map_ids(&root, &mut seen, 0);
        }
    };
    let read = host as &dyn mod_api::Host;
    for (_, stack) in player.inventory_slots(read).unwrap_or_default() {
        look(&stack);
    }
    if let Some(menu) = player.container_menu(read)
        && let Some(count) = menu.slot_count(read)
    {
        for slot in 0..count {
            if let Some(Some(stack)) = menu.slot(read, slot) {
                look(&stack);
            }
        }
    }
    seen.contains(&id)
}

/// Client: one slice of a map's colours. Once all slices are in, keep the picture as RGBA.
fn on_map_data(data: MapData, _: &mut dyn IPayloadContext) -> Result<(), String> {
    let id = data.id.0;
    let part = data.part.0;
    if part < 0 || part as usize >= PARTS || data.colors.len() != PART_LEN {
        return Ok(());
    }
    let part = part as usize;
    vanilla_map_info(id, data.scale.0.clamp(0, 4) as i8, data.locked);
    with_cache(|cache| {
        let (colors, got) = cache
            .partial
            .entry(id)
            .or_insert_with(|| (vec![0; MAP_SIZE * MAP_SIZE], 0));
        colors[part * PART_LEN..(part + 1) * PART_LEN].copy_from_slice(&data.colors);
        *got |= 1 << part;
        if *got as usize != (1 << PARTS) - 1 {
            return;
        }
        let Some((colors, _)) = cache.partial.remove(&id) else {
            return;
        };
        // Without the palette yet the picture is dropped; it is asked for again later.
        cache.store(id, &colors);
    });
    Ok(())
}
