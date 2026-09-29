//! Redstone contraptions built in a real in-process `World` (flat stone, no players) and advanced
//! one game tick at a time. Expected values and timings follow the vanilla 26.3 server.

use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use pumpkin::block::blocks::redstone::lever::LeverLikePropertiesExt;
use pumpkin::block::entities::BlockEntity;
use pumpkin::data::VanillaData;
use pumpkin::entity::Entity;
use pumpkin::entity::item::ItemEntity;
use pumpkin::server::Server;
use pumpkin::world::World;
use pumpkin_config::{AdvancedConfiguration, BasicConfiguration, TelemetryConfig};
use pumpkin_data::block_properties::{
    AttachFace, ComparatorLikeProperties, Facing, HorizontalFacing, LeverLikeProperties,
    ModeComparator, ObserverLikeProperties, PistonHeadLikeProperties, PistonType,
    RedstoneOreLikeProperties, RedstoneWireLikeProperties, RepeaterLikeProperties,
    StickyPistonLikeProperties,
};
use pumpkin_data::dimension::Dimension;
use pumpkin_data::entity::EntityType;
use pumpkin_data::item::Item;
use pumpkin_data::item_stack::ItemStack;
use pumpkin_data::{Block, BlockStateId};
use pumpkin_inventory::Inventory;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector2::Vector2;
use pumpkin_util::math::vector3::Vector3;
use pumpkin_util::world_seed::Seed;
use pumpkin_world::generation::generator::{FlatLayer, WorldGenerator, flat::FlatGenerator};
use pumpkin_world::tick::TickPriority;
use pumpkin_world::world::BlockFlags;

/// Far from spawn, so none of these chunks exist before the flat generator is swapped in.
const ORIGIN_X: i32 = 1032;
const ORIGIN_Z: i32 = 1032;
/// The flat world is stone up to y = -1; contraptions stand on it at y = 0.
const FLOOR_TOP: i32 = -1;
/// Chunks around the origin kept loaded and ticking, like vanilla `/forceload` does.
const AREA_CHUNK_RADIUS: i32 = 2;
/// Ticks run after building and before triggering, so placement side effects have settled. The
/// longest one is the pulse an observer fires when placed like `/setblock` (4 ticks in vanilla);
/// this leaves room for it to run late.
const SETTLE_TICKS: u32 = 10;

const fn at(dx: i32, dy: i32, dz: i32) -> BlockPos {
    BlockPos::new(ORIGIN_X + dx, FLOOR_TOP + 1 + dy, ORIGIN_Z + dz)
}

struct Harness {
    server: Arc<Server>,
    world: Arc<World>,
    _dir: tempfile::TempDir,
}

impl Harness {
    #[expect(
        clippy::expect_used,
        reason = "a harness that cannot start has nothing to test"
    )]
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp world dir");
        let basic_config = BasicConfiguration {
            seed: Seed(0),
            allow_nether: false,
            allow_end: false,
            default_level_name: dir.path().join("world").to_string_lossy().into_owned(),
            ..BasicConfiguration::default()
        };
        let mut advanced_config = AdvancedConfiguration::default();
        advanced_config.networking.bedrock.online_mode = false;
        let telemetry_config = TelemetryConfig {
            enabled: false,
            ..TelemetryConfig::default()
        };
        let vanilla_data = VanillaData {
            banned_ip_list: RwLock::default(),
            banned_player_list: RwLock::default(),
            operator_config: RwLock::default(),
            user_cache: RwLock::default(),
            whitelist_config: RwLock::default(),
        };
        let server = Server::new(
            basic_config,
            advanced_config,
            telemetry_config,
            vanilla_data,
        )
        .await
        .expect("server starts");
        let world = server.get_world_from_dimension(&Dimension::OVERWORLD);
        world
            .level
            .set_world_gen(Arc::new(WorldGenerator::Flat(Box::new(
                FlatGenerator::new(
                    Seed(0),
                    Dimension::OVERWORLD,
                    vec![FlatLayer {
                        block: "minecraft:stone".to_string(),
                        height: FLOOR_TOP + 65,
                    }],
                    "minecraft:plains".to_string(),
                ),
            ))));

        let harness = Self {
            server,
            world,
            _dir: dir,
        };
        harness.load_area().await;
        harness
    }

    async fn load_area(&self) {
        let center = Vector2::new(ORIGIN_X >> 4, ORIGIN_Z >> 4);
        let chunks: Vec<Vector2<i32>> = (-AREA_CHUNK_RADIUS..=AREA_CHUNK_RADIUS)
            .flat_map(|dx| {
                (-AREA_CHUNK_RADIUS..=AREA_CHUNK_RADIUS)
                    .map(move |dz| Vector2::new(center.x + dx, center.y + dz))
            })
            .collect();
        self.add_force_tickets(&chunks);
        for chunk in &chunks {
            let loaded = tokio::time::timeout(
                Duration::from_secs(120),
                self.world.level.get_or_fetch_chunk(*chunk, |_| ()),
            )
            .await;
            assert!(loaded.is_ok(), "chunk {chunk:?} did not load");
        }
        self.world
            .forced_chunks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(chunks.iter().copied());
        self.world.update_active_chunks();
    }

    fn add_force_tickets(&self, chunks: &[Vector2<i32>]) {
        let mut loading = self
            .world
            .level
            .chunk_loading
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for chunk in chunks {
            loading.add_force_ticket(*chunk);
        }
        loading.send_change();
    }

    /// Places a block like vanilla `/setblock`: shape it from its neighbours, then set it with
    /// block updates.
    fn place(&self, pos: &BlockPos, state: BlockStateId) {
        let shaped = self.world.update_from_neighbor_shapes(state, pos);
        self.world
            .set_block_state(pos, shaped, BlockFlags::NOTIFY_ALL);
        assert!(
            self.block(pos) == Block::from_state_id(state),
            "harness could not place {} at {pos:?}, found {}",
            Block::from_state_id(state).name,
            self.describe(pos)
        );
    }

    fn block(&self, pos: &BlockPos) -> &'static Block {
        self.world.get_block(pos)
    }

    fn state(&self, pos: &BlockPos) -> BlockStateId {
        self.world.get_block_state_id(pos)
    }

    fn describe(&self, pos: &BlockPos) -> String {
        let (block, state) = self.world.get_block_and_state_id(pos);
        let props = block
            .properties(state)
            .map(|props| {
                props
                    .to_props()
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        format!("{}[{props}]", block.name)
    }

    fn tick(&self) {
        self.world.tick(&self.server);
    }

    fn ticks(&self, count: u32) {
        for _ in 0..count {
            self.tick();
        }
    }

    /// Ticks up to `max` times and returns the first tick (1-based) after which `done` holds.
    fn first_tick_where(&self, max: u32, done: impl Fn(&Self) -> bool) -> Option<u32> {
        (1..=max).find(|_| {
            self.tick();
            done(self)
        })
    }

    /// Ticks `count` times and records `sample` after each tick.
    fn timeline<T>(&self, count: u32, sample: impl Fn(&Self) -> T) -> Vec<T> {
        (0..count)
            .map(|_| {
                self.tick();
                sample(self)
            })
            .collect()
    }

    /// Same steps as the private `toggle_lever` in `block/blocks/redstone/lever.rs`, minus the
    /// sound and game event.
    fn toggle_lever(&self, pos: &BlockPos) {
        let (block, state) = self.world.get_block_and_state_id(pos);
        assert!(block == &Block::LEVER, "no lever at {pos:?}");
        let mut props = LeverLikeProperties::from_state_id(state);
        props.powered = !props.powered;
        self.world
            .set_block_state(pos, props.to_state_id(block), BlockFlags::NOTIFY_ALL);
        self.world.update_neighbors(pos, None);
        self.world.update_neighbors(
            &pos.offset(props.get_direction().opposite().to_offset()),
            None,
        );
    }

    fn wire_power(&self, pos: &BlockPos) -> Option<u8> {
        (self.block(pos) == &Block::REDSTONE_WIRE)
            .then(|| RedstoneWireLikeProperties::from_state_id(self.state(pos)).power)
    }

    fn inventory(&self, pos: &BlockPos) -> Option<Arc<dyn Inventory>> {
        self.world
            .get_block_entity(pos)
            .and_then(BlockEntity::get_inventory)
    }

    /// Total count of `item` across all slots of the container at `pos`.
    fn count_items(&self, pos: &BlockPos, item: &Item) -> u32 {
        self.inventory(pos).map_or(0, |inventory| {
            (0..inventory.size())
                .map(|slot| inventory.get_stack(slot))
                .filter(|stack| stack.get_item().id == item.id)
                .map(|stack| u32::from(stack.item_count))
                .sum()
        })
    }
}

fn floor_lever() -> BlockStateId {
    LeverLikeProperties {
        face: AttachFace::Floor,
        facing: HorizontalFacing::North,
        powered: false,
    }
    .to_state_id(&Block::LEVER)
}

/// A lever on the side of the block it points away from.
fn wall_lever(facing: HorizontalFacing) -> BlockStateId {
    LeverLikeProperties {
        face: AttachFace::Wall,
        facing,
        powered: false,
    }
    .to_state_id(&Block::LEVER)
}

fn wire() -> BlockStateId {
    Block::REDSTONE_WIRE.default_state.id
}

// ---------------------------------------------------------------------------
// 0. Tick scheduler (no block logic involved)
// ---------------------------------------------------------------------------

/// Vanilla `LevelTicks`: a tick scheduled between ticks with delay `d` triggers at
/// `game time + d`, so it runs during the d-th following tick.
#[tokio::test(flavor = "multi_thread")]
async fn scheduled_block_tick_runs_after_exactly_its_delay() {
    let h = Harness::new().await;
    let pos = at(0, 0, 0);
    h.place(&pos, Block::STONE.default_state.id);
    h.ticks(SETTLE_TICKS);

    let delays = [1u8, 2, 3, 4, 8];
    let ran_at: Vec<Option<u32>> = delays
        .iter()
        .map(|&delay| {
            h.world
                .schedule_block_tick(&Block::STONE, pos, delay, TickPriority::Normal);
            assert!(h.world.is_block_tick_scheduled(&pos, &Block::STONE));
            h.first_tick_where(u32::from(delay) + 4, |h| {
                !h.world.is_block_tick_scheduled(&pos, &Block::STONE)
            })
        })
        .collect();
    let expected: Vec<Option<u32>> = delays.iter().map(|&d| Some(u32::from(d))).collect();
    assert_eq!(
        ran_at, expected,
        "tick (after scheduling) in which a block tick with delay {delays:?} ran"
    );
}

// ---------------------------------------------------------------------------
// 1. Redstone wire
// ---------------------------------------------------------------------------

/// Lever at x=0, wire from x=1 to x=17. Index 0 is the wire touching the lever.
fn build_lever_and_wire_line(h: &Harness) -> (BlockPos, Vec<BlockPos>) {
    let lever = at(0, 0, 0);
    h.place(&lever, floor_lever());
    let wires: Vec<BlockPos> = (1..=17).map(|x| at(x, 0, 0)).collect();
    for pos in &wires {
        h.place(pos, wire());
    }
    (lever, wires)
}

#[tokio::test(flavor = "multi_thread")]
async fn wire_loses_one_signal_level_per_block() {
    let h = Harness::new().await;
    let (lever, wires) = build_lever_and_wire_line(&h);
    h.ticks(SETTLE_TICKS);

    h.toggle_lever(&lever);

    // Wire updates are immediate in vanilla, so no tick is needed.
    let actual: Vec<Option<u8>> = wires.iter().map(|pos| h.wire_power(pos)).collect();
    let expected: Vec<Option<u8>> = (0..17u8).map(|n| Some(15u8.saturating_sub(n))).collect();
    assert_eq!(
        actual, expected,
        "wire power by distance from the lever (index 0 touches it)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wire_loses_all_power_when_lever_turns_off() {
    let h = Harness::new().await;
    let (lever, wires) = build_lever_and_wire_line(&h);
    h.ticks(SETTLE_TICKS);

    h.toggle_lever(&lever);
    h.ticks(2);
    h.toggle_lever(&lever);

    let actual: Vec<Option<u8>> = wires.iter().map(|pos| h.wire_power(pos)).collect();
    assert_eq!(
        actual,
        vec![Some(0); wires.len()],
        "wire power after lever off"
    );
}

// ---------------------------------------------------------------------------
// 2. Repeater delays
// ---------------------------------------------------------------------------

/// Lever -> repeater -> wire along +x. A repeater with `delay` N must turn on exactly 2N game ticks
/// after its input rises (vanilla `DiodeBlock.checkTickOnNeighbor` schedules `DELAY * 2`).
async fn assert_repeater_output_delay(delay: u8) {
    let h = Harness::new().await;
    let lever = at(0, 0, 0);
    let repeater = at(1, 0, 0);
    let output = at(2, 0, 0);
    h.place(&lever, floor_lever());
    h.place(
        &repeater,
        RepeaterLikeProperties {
            delay,
            facing: HorizontalFacing::West,
            locked: false,
            powered: false,
        }
        .to_state_id(&Block::REPEATER),
    );
    h.place(&output, wire());
    h.ticks(SETTLE_TICKS);
    assert_eq!(
        h.wire_power(&output),
        Some(0),
        "output lit before the lever"
    );

    h.toggle_lever(&lever);

    let expected = u32::from(delay) * 2;
    let timeline = h.timeline(expected + 4, |h| {
        (
            RepeaterLikeProperties::from_state_id(h.state(&repeater)).powered,
            h.wire_power(&output),
        )
    });
    let rose_at = timeline
        .iter()
        .position(|(_, power)| *power == Some(15))
        .map(|index| index as u32 + 1);
    assert_eq!(
        rose_at,
        Some(expected),
        "delay {delay} repeater: game tick at which the output wire reached 15; \
         (repeater powered, output power) per tick: {timeline:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn repeater_delay_1_outputs_after_2_game_ticks() {
    assert_repeater_output_delay(1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn repeater_delay_2_outputs_after_4_game_ticks() {
    assert_repeater_output_delay(2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn repeater_delay_3_outputs_after_6_game_ticks() {
    assert_repeater_output_delay(3).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn repeater_delay_4_outputs_after_8_game_ticks() {
    assert_repeater_output_delay(4).await;
}

// ---------------------------------------------------------------------------
// 3. Redstone torch
// ---------------------------------------------------------------------------

/// Torch standing on a stone block, with a wall lever on the block's east face.
fn build_torch_on_block(h: &Harness) -> (BlockPos, BlockPos) {
    let block = at(0, 0, 0);
    let torch = at(0, 1, 0);
    let lever = at(1, 0, 0);
    h.place(&block, Block::STONE.default_state.id);
    h.place(&torch, Block::REDSTONE_TORCH.default_state.id);
    h.place(&lever, wall_lever(HorizontalFacing::East));
    (torch, lever)
}

fn torch_lit(h: &Harness, pos: &BlockPos) -> bool {
    RedstoneOreLikeProperties::from_state_id(h.state(pos)).lit
}

#[tokio::test(flavor = "multi_thread")]
async fn redstone_torch_turns_off_two_ticks_after_its_block_is_powered() {
    let h = Harness::new().await;
    let (torch, lever) = build_torch_on_block(&h);
    h.ticks(SETTLE_TICKS);
    assert!(
        torch_lit(&h, &torch),
        "torch on an unpowered block must be lit"
    );

    h.toggle_lever(&lever);

    // Vanilla `RedstoneTorchBlock.neighborChanged` schedules the toggle 2 ticks out.
    let off_at = h.first_tick_where(10, |h| !torch_lit(h, &torch));
    assert_eq!(off_at, Some(2), "game tick at which the torch turned off");
}

#[tokio::test(flavor = "multi_thread")]
async fn redstone_torch_relights_two_ticks_after_power_is_removed() {
    let h = Harness::new().await;
    let (torch, lever) = build_torch_on_block(&h);
    h.ticks(SETTLE_TICKS);
    h.toggle_lever(&lever);
    h.ticks(SETTLE_TICKS);
    assert!(
        !torch_lit(&h, &torch),
        "torch on a powered block must be off"
    );

    h.toggle_lever(&lever);

    let lit_at = h.first_tick_where(10, |h| torch_lit(h, &torch));
    assert_eq!(lit_at, Some(2), "game tick at which the torch relit");
}

// ---------------------------------------------------------------------------
// 4. Observer
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn observer_pulses_for_two_ticks_when_block_in_front_changes() {
    let h = Harness::new().await;
    let observer = at(0, 0, 0);
    let watched = at(1, 0, 0);
    let output = at(-1, 0, 0);
    h.place(
        &observer,
        ObserverLikeProperties {
            facing: Facing::East,
            powered: false,
        }
        .to_state_id(&Block::OBSERVER),
    );
    h.place(&output, wire());
    h.ticks(SETTLE_TICKS);
    assert!(
        !ObserverLikeProperties::from_state_id(h.state(&observer)).powered,
        "observer still pulsing from its own placement after {SETTLE_TICKS} ticks"
    );

    h.place(&watched, Block::STONE.default_state.id);

    // Vanilla: `updateShape` schedules the tick 2 out, the tick powers the observer and schedules
    // the unpower 2 further out.
    let timeline = h.timeline(8, |h| {
        (
            ObserverLikeProperties::from_state_id(h.state(&observer)).powered,
            h.wire_power(&output),
        )
    });
    let powered: Vec<bool> = timeline.iter().map(|(powered, _)| *powered).collect();
    assert_eq!(
        powered,
        vec![false, true, true, false, false, false, false, false],
        "observer powered after each game tick; (powered, output wire) per tick: {timeline:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. Comparator
// ---------------------------------------------------------------------------

const COMPARATOR_REAR: u8 = 12;
const COMPARATOR_SIDE: u8 = 10;

/// Comparator facing west (rear input at -x, output at +x). The rear is fed by a wire run from a
/// redstone block that arrives at 12, the south side by one that arrives at 10.
fn build_comparator(h: &Harness, mode: ModeComparator) -> BlockPos {
    let comparator = at(0, 0, 0);
    let output = at(1, 0, 0);
    h.place(
        &comparator,
        ComparatorLikeProperties {
            facing: HorizontalFacing::West,
            mode,
            powered: false,
        }
        .to_state_id(&Block::COMPARATOR),
    );
    h.place(&output, wire());

    // Vanilla only re-evaluates a comparator on a neighbour update, so its inputs go in after it.
    let rear_len = i32::from(15 - COMPARATOR_REAR) + 1;
    let side_len = i32::from(15 - COMPARATOR_SIDE) + 1;
    for step in 1..=rear_len {
        h.place(&at(-step, 0, 0), wire());
    }
    for step in 1..=side_len {
        h.place(&at(0, 0, step), wire());
    }
    h.place(
        &at(-rear_len - 1, 0, 0),
        Block::REDSTONE_BLOCK.default_state.id,
    );
    h.place(
        &at(0, 0, side_len + 1),
        Block::REDSTONE_BLOCK.default_state.id,
    );

    assert_eq!(
        h.wire_power(&at(-1, 0, 0)),
        Some(COMPARATOR_REAR),
        "rear input"
    );
    assert_eq!(
        h.wire_power(&at(0, 0, 1)),
        Some(COMPARATOR_SIDE),
        "side input"
    );
    output
}

#[tokio::test(flavor = "multi_thread")]
async fn comparator_compare_mode_passes_rear_signal() {
    let h = Harness::new().await;
    let output = build_comparator(&h, ModeComparator::Compare);
    h.ticks(10);
    assert_eq!(
        h.wire_power(&output),
        Some(COMPARATOR_REAR),
        "compare mode with rear {COMPARATOR_REAR} >= side {COMPARATOR_SIDE}; comparator is {}",
        h.describe(&at(0, 0, 0))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn comparator_subtract_mode_outputs_rear_minus_side() {
    let h = Harness::new().await;
    let output = build_comparator(&h, ModeComparator::Subtract);
    h.ticks(10);
    assert_eq!(
        h.wire_power(&output),
        Some(COMPARATOR_REAR - COMPARATOR_SIDE),
        "subtract mode {COMPARATOR_REAR} - {COMPARATOR_SIDE}; comparator is {}",
        h.describe(&at(0, 0, 0))
    );
}

// ---------------------------------------------------------------------------
// 6. Pistons
// ---------------------------------------------------------------------------

/// Enough for vanilla's block event plus the two moving-piston progress steps and the final tick.
const PISTON_TICKS: u32 = 10;

fn piston_state(block: &Block, extended: bool) -> BlockStateId {
    StickyPistonLikeProperties {
        extended,
        facing: Facing::East,
    }
    .to_state_id(block)
}

/// Piston at the origin pushing east, a gold block in front of it, a lever behind it.
fn build_piston(h: &Harness, piston: &Block) -> BlockPos {
    let lever = at(-1, 0, 0);
    h.place(&at(0, 0, 0), piston_state(piston, false));
    h.place(&at(1, 0, 0), Block::GOLD_BLOCK.default_state.id);
    h.place(&lever, floor_lever());
    h.ticks(SETTLE_TICKS);
    lever
}

fn piston_row(h: &Harness) -> String {
    format!(
        "{} | {} | {}",
        h.describe(&at(0, 0, 0)),
        h.describe(&at(1, 0, 0)),
        h.describe(&at(2, 0, 0))
    )
}

fn assert_extended(h: &Harness, piston: &Block, trace: &[String]) {
    let head = at(1, 0, 0);
    let head_props = PistonHeadLikeProperties::from_state_id(h.state(&head));
    let head_type = if piston == &Block::STICKY_PISTON {
        PistonType::Sticky
    } else {
        PistonType::Normal
    };
    let ok = h.state(&at(0, 0, 0)) == piston_state(piston, true)
        && h.block(&head) == &Block::PISTON_HEAD
        && head_props.facing == Facing::East
        && head_props.r#type == head_type
        && h.block(&at(2, 0, 0)) == &Block::GOLD_BLOCK;
    assert!(
        ok,
        "expected extended {} | piston_head[facing=east,type={head_type:?}] | gold_block, \
         per tick:\n{}",
        piston.name,
        trace.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn piston_extends_and_pushes_block_in_front() {
    let h = Harness::new().await;
    let lever = build_piston(&h, &Block::PISTON);

    h.toggle_lever(&lever);

    let trace = h.timeline(PISTON_TICKS, piston_row);
    assert_extended(&h, &Block::PISTON, &trace);
}

#[tokio::test(flavor = "multi_thread")]
async fn piston_retracts_and_leaves_block_when_unpowered() {
    let h = Harness::new().await;
    let lever = build_piston(&h, &Block::PISTON);
    h.toggle_lever(&lever);
    let trace = h.timeline(PISTON_TICKS, piston_row);
    assert_extended(&h, &Block::PISTON, &trace);

    h.toggle_lever(&lever);

    let trace = h.timeline(PISTON_TICKS, piston_row);
    let ok = h.state(&at(0, 0, 0)) == piston_state(&Block::PISTON, false)
        && h.block(&at(1, 0, 0)) == &Block::AIR
        && h.block(&at(2, 0, 0)) == &Block::GOLD_BLOCK;
    assert!(
        ok,
        "expected retracted piston | air | gold_block, per tick:\n{}",
        trace.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sticky_piston_pulls_block_back_when_unpowered() {
    let h = Harness::new().await;
    let lever = build_piston(&h, &Block::STICKY_PISTON);
    h.toggle_lever(&lever);
    let trace = h.timeline(PISTON_TICKS, piston_row);
    assert_extended(&h, &Block::STICKY_PISTON, &trace);

    h.toggle_lever(&lever);

    let trace = h.timeline(PISTON_TICKS, piston_row);
    let ok = h.state(&at(0, 0, 0)) == piston_state(&Block::STICKY_PISTON, false)
        && h.block(&at(1, 0, 0)) == &Block::GOLD_BLOCK
        && h.block(&at(2, 0, 0)) == &Block::AIR;
    assert!(
        ok,
        "expected retracted sticky_piston | gold_block | air, per tick:\n{}",
        trace.join("\n")
    );
}

// ---------------------------------------------------------------------------
// 7. Quasi-connectivity
// ---------------------------------------------------------------------------

/// A redstone block two above the piston powers the space above it, which vanilla
/// `PistonBaseBlock.getNeighborSignal` also counts, but nothing touches the piston itself.
fn build_quasi_connected_piston(h: &Harness) {
    h.place(&at(0, 0, 0), piston_state(&Block::PISTON, false));
    h.place(&at(1, 0, 0), Block::GOLD_BLOCK.default_state.id);
    h.ticks(SETTLE_TICKS);
    h.place(&at(0, 2, 0), Block::REDSTONE_BLOCK.default_state.id);
}

#[tokio::test(flavor = "multi_thread")]
async fn quasi_connected_piston_extends_on_next_block_update() {
    let h = Harness::new().await;
    build_quasi_connected_piston(&h);
    h.ticks(SETTLE_TICKS);

    // Any block update next to the piston makes it re-check its power.
    h.place(&at(0, 0, -1), Block::STONE.default_state.id);

    let trace = h.timeline(PISTON_TICKS, piston_row);
    assert_extended(&h, &Block::PISTON, &trace);
}

#[tokio::test(flavor = "multi_thread")]
async fn quasi_connected_piston_waits_for_a_block_update() {
    let h = Harness::new().await;
    build_quasi_connected_piston(&h);

    // Placing the redstone block only updates its own neighbours, none of which is the piston.
    let trace = h.timeline(PISTON_TICKS, piston_row);
    assert!(
        h.state(&at(0, 0, 0)) == piston_state(&Block::PISTON, false),
        "piston moved without a block update, per tick:\n{}",
        trace.join("\n")
    );
}

// ---------------------------------------------------------------------------
// 8. Hopper
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn hopper_moves_item_into_chest_below() {
    let h = Harness::new().await;
    let chest = at(0, 0, 0);
    let hopper = at(0, 1, 0);
    h.place(&chest, Block::CHEST.default_state.id);
    h.place(&hopper, Block::HOPPER.default_state.id);
    h.ticks(SETTLE_TICKS);
    let hopper_inventory = h.inventory(&hopper);
    assert!(hopper_inventory.is_some(), "hopper has no block entity");
    assert!(h.inventory(&chest).is_some(), "chest has no block entity");
    if let Some(inventory) = hopper_inventory {
        inventory.set_stack(0, ItemStack::new(1, &Item::DIAMOND));
    }

    // Vanilla moves it on the first tick: a hopper with nothing to do is never on cooldown.
    let moved_at = h.first_tick_where(8, |h| h.count_items(&chest, &Item::DIAMOND) == 1);
    assert!(
        moved_at.is_some(),
        "diamond not in chest within 8 ticks; hopper has {}, chest has {}",
        h.count_items(&hopper, &Item::DIAMOND),
        h.count_items(&chest, &Item::DIAMOND)
    );
    assert_eq!(
        h.count_items(&hopper, &Item::DIAMOND),
        0,
        "hopper kept a copy of the moved diamond"
    );
}

/// Places a hopper, then drops a still diamond item entity `height` blocks above its base.
/// Returns the hopper position and the item entity.
fn hopper_with_item_above(h: &Harness, height: f64) -> (BlockPos, Arc<ItemEntity>) {
    let hopper = at(0, 0, 0);
    h.place(&hopper, Block::HOPPER.default_state.id);
    h.ticks(SETTLE_TICKS);

    let spawn = Vector3::new(
        f64::from(hopper.0.x) + 0.5,
        f64::from(hopper.0.y) + height,
        f64::from(hopper.0.z) + 0.5,
    );
    let item = Arc::new(ItemEntity::new_with_velocity(
        Entity::new(h.world.clone(), spawn, &EntityType::ITEM),
        ItemStack::new(1, &Item::DIAMOND),
        Vector3::new(0.0, 0.0, 0.0),
        ItemEntity::DEFAULT_PICKUP_DELAY,
    ));
    assert!(
        h.world.spawn_entity(item.clone()),
        "item spawn was cancelled"
    );
    (hopper, item)
}

fn assert_hopper_picked_up(h: &Harness, hopper: &BlockPos, item: &ItemEntity, max_ticks: u32) {
    let trace = h.timeline(max_ticks, |h| {
        (
            h.count_items(hopper, &Item::DIAMOND),
            item.get_entity().pos.load().y - f64::from(hopper.0.y),
        )
    });
    let entity = item.get_entity();
    assert!(
        trace.iter().any(|(count, _)| *count == 1),
        "hopper did not pick up the item within {max_ticks} ticks; entity removed={}; \
         (diamonds in hopper, item y above hopper base) per tick: {trace:?}",
        entity.removed.load(std::sync::atomic::Ordering::Relaxed),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn hopper_picks_up_item_entity_above_it() {
    let h = Harness::new().await;
    let (hopper, item) = hopper_with_item_above(&h, 1.5);

    // Vanilla picks it up on the first tick: y + 1.5 is inside the hopper's suck box
    // (y + 11/16 up to y + 2), and hoppers ignore the pickup delay.
    assert_hopper_picked_up(&h, &hopper, &item, 40);
}

#[tokio::test(flavor = "multi_thread")]
async fn hopper_picks_up_item_that_falls_into_it() {
    let h = Harness::new().await;
    let (hopper, item) = hopper_with_item_above(&h, 3.0);

    // Vanilla item gravity brings it into the suck box on about the 7th tick.
    assert_hopper_picked_up(&h, &hopper, &item, 40);
}
