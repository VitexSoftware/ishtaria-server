# ishtaria-server

Authoritative world server of Ishtaria: one planet, its simulation, persistence and the federation endpoint that links worlds through portals.

**Status:** PostgreSQL-backed world identity, heightmaps, characters, inventory, eating, permanent death, memorial APIs and server-authoritative walking and jumping onto scenery. General rigid-body physics, free-roaming enemies and player-versus-player combat, travel tickets and cross-world play are not implemented yet; portals are built alone and linked with portals of other worlds by share links.

## Running Locally

Create a database owned by the user running the server:

```sh
createdb ishtaria
cargo run -- etc/server.toml --import ../ishtaria-worldgen/examples/planet-seed42-256.pgm --seed 42
```

The bundled configuration uses the local PostgreSQL Unix socket with peer authentication. For another database, set `database_url` in the configuration or override it with `DATABASE_URL`. Do not commit passwords. Under systemd, provision a PostgreSQL role and database for the `ishtaria` service user.

The server applies the versioned SQL migrations in `migrations/` automatically before accepting requests. Migrations are embedded in the executable; add a new migration for schema changes rather than editing one already applied. Keep `Cargo.lock` in version control for reproducible `--locked` builds.

Import accepts the worldgen binary P5, 8-bit grayscale strip of six square faces (up to 1024 pixels per face and 16 MiB per file). Both the original PGM and decoded pixels are stored in PostgreSQL, with a SHA-256 checksum and the explicitly supplied seed. PGM contains no seed, so the server cannot verify that it matches the generator input. An identical import is a no-op; a different map, seed or ruleset is rejected instead of silently replacing world data. Invalid imports roll back the world transaction.

Subsequent starts use the stored data without needing the source file:

```sh
cargo run -- etc/server.toml
curl --fail http://localhost:7400/health
curl --fail http://localhost:7400/world
curl --fail http://localhost:7400/terrain/0/128/128
curl --fail http://localhost:7400/world/heightmap -o planet.pgm
```

`GET /health` checks PostgreSQL availability; `/world` returns world metadata (including `server_version`, e.g. `0.1.0`); `/world/heightmap` downloads the original PGM; `/terrain/{face}/{x}/{y}` returns an 8-bit height sample. Face indices follow the worldgen strip order: +X, -X, +Y, -Y, +Z, -Z; coordinates are zero-based, with y increasing downwards. This raw sample endpoint does not infer metres because PGM does not store amplitude. Missing maps or out-of-bounds coordinates return 404; invalid coordinates return 400; database failures return 503. The world/terrain API is read-only and unauthenticated. Restrict network access or use a reverse proxy before exposing it publicly. SIGINT and SIGTERM shut the server down gracefully.

### Solar Clock

`GET /world` includes `solar` version 1: current server `unix_seconds`, a unit
`direction` toward the Sun in planet-fixed coordinates, `sidereal_day_seconds`
(86164.0905) and `angular_radius_degrees`. It is computed for each response;
no migration, world reset or player update is needed. The server's UTC clock
must be accurate. Clients extrapolate using elapsed monotonic time, not their
local wall clock.

The Earth-like ephemeris uses days from J2000, mean solar longitude/anomaly,
orbital eccentricity, approximately 23.44-degree obliquity, right ascension,
declination and Greenwich sidereal rotation. +Y is north, +X is longitude zero,
and +Z is east. Mean solar days last 24 real hours; local sunrise/sunset and
day length vary with latitude and season, including polar day/night. Sun radius
varies with orbital distance around 0.2666 degrees. The world stays in its
fixed coordinate system, so saved player positions do not rotate or change.

For a temporary administrative daylight adjustment, start the server with
`--solar-offset-seconds 43200` (12 hours). The bounded range is -86400 to 86400;
the default is zero. This shifts only the announced solar clock for that run,
which continues at normal speed. It does not change OS/PostgreSQL time,
session expiry, survival, player positions or stored world data. Restarting
without the option restores UTC-based solar time.

This is a low-order solar approximation using UTC as UT1, not an N-body
simulation or precision observatory ephemeris. Lunar motion, eclipses, weather
and seasonal terrain/temperature simulation are not implemented.

### Surface Environment

`GET /world/environment` derives a deterministic version-2 environment from the
persisted heightmap, without changing it or adding game entities. The response
includes `heightmap_sha256` and the exact decimal-string `seed`; clients must
verify both against `/world`. It uses the default worldgen amplitude convention
of 8000 metres, not an amplitude inferred from arbitrary PGM input.

Version 2 announces continuous seeded local relief, bounded to +/-12 metres,
added to bilinear base-map sampling for walking, scenery and new spawns. The
formula in `environment::relief_offset` uses normalized global coordinates,
not cube-face coordinates, so its detail has no face seams. It smoothly fades
out from 150 metres down to sea level. Coarse hydrology and `elevation_m` remain
base-map data; the persisted PGM, seed and checksum are unchanged. Version-1
clients must reject the new environment rather than render mismatched heights.
Existing player records are not reset; the next accepted move projects their
position onto the version-2 surface.

`face_size` is bounded to 64. Arrays use face-major order, then row and column,
unlike the horizontal PNG strip. `elevation_m` contains terrain heights;
`water_m` contains sea/lake/drainage levels, or `-8001` for dry cells.
`downstream` contains a cell index or `-1` at an outlet; `flow` is upstream cell
count. `biomes` uses `0=ocean`, `1=lake`, `2=river`, `3=beach`, `4=grassland`,
`5=forest`, `6=mountain`, `7=snow`.

Priority-flood depression filling determines lake spill levels and an acyclic
drainage tree across cube-face edges. Accumulated catchments select river cells;
latitude, elevation and seeded moisture select land biomes. Worlds without sea
drain to their global minimum rather than inventing an ocean. Computation is
bounded and runs outside the async request worker. Missing maps return 404.
This is coarse preview hydrology, not water physics, erosion or persistent
harvestable objects. Rebuild/restart the server to expose this new endpoint;
no new migration or reimport is required for an existing world.

This imported map is a finite preview, not a full Earth-resolution terrain database. Procedural world generation remains the source of base terrain; future player changes require separate delta migrations.

## Player Accounts

Migration `0003_players.sql` adds world-scoped accounts and hashed sessions;
`0004_player_character.sql` adds the character identifier. Existing accounts
receive `retro/humanMaleA` without changing their balances. New players receive
100 gold only on insertion. No API accepts client-supplied balances or stats.

- `POST /players`: JSON `username`, `password` and optional `character`; returns
  HTTP 201 with `token` and `player`. The default character is `retro/humanMaleA`.
- `POST /players/login`: JSON `username` and `password`; returns the same session
  shape for a living character. Correct credentials for a deceased character
  return HTTP 410 with `obituary`, never a token or authenticated profile.
- `GET /players/me`: authenticated player profile.
- `DELETE /players/session`: revokes the supplied session, returning HTTP 204.

Authenticated requests use `Authorization: Bearer TOKEN`. Player JSON contains
`username`, `character` and `stats` (gold, health, stamina, food, water, level and
experience). Gold is a decimal string to preserve PostgreSQL BIGINT precision.
The character whitelist matches the client catalog: four appearances in each
of `protagonists`, `retro` and `survivors`. Unknown IDs are rejected; character
selection is performed at registration, not by changing local preferences.

Player JSON also includes `position`: Cartesian `x`, `y`, `z` in metres from
the planet centre, with a mean radius of 6,371,000 metres. Registration, or the
first successful living login with no saved position, randomly selects a safe
dry grassland/forest cell and persists it transactionally in the existing position
columns. The surrounding 3-by-3 environment cells must also be dry lowlands,
excluding oceans, lakes, rivers, beaches, mountains and snow. Original heightmap
samples must remain 150-1600 metres above sea level with slope at most 20%.
Coordinates use the same cube-sphere geometry and bilinear height sampling as
the client, including terrain elevation rather than just the planet radius.
Without an imported map or a safe candidate, registration returns HTTP 503 and
rolls back the player and inventory grant; no ocean fallback is allowed.
Later logins and profile refreshes preserve the saved position. Simultaneous
first logins share one stored location; deceased logins do not receive a new one.
Existing saved positions are not automatically relocated by this change.
Clients cannot set position through the account API. Unpositioned records returned
directly by a profile query have `position: null` until their next successful login.

### Surface Movement

Migration `0009_player_movement.sql` adds a nonnegative movement sequence and
server timestamp without replacing world data, positions or balances.
`position` includes `sequence`, an exact decimal string, and boolean `airborne`
and `on_object` flags. Migration `0010_player_jumping.sql` persists flight velocity
and support state without changing existing coordinates or balances.
Migration `0011_player_running.sql` permits running launch speeds without changing saved positions.

`POST /players/me/move` requires a living authenticated character and accepts:

```json
{"direction": [0.0, 0.0, 1.0], "sequence": "1", "jump": true, "run": true}
```

`direction` is a finite planet-fixed vector of length at most one, projected
onto the local tangent plane. `sequence` must be a canonical positive decimal
string within signed BIGINT range and greater than the last accepted sequence.
Clients cannot supply position, elapsed time, player ID or balances.
The reply contains `position` and boolean `moving`.
`jump` is optional and defaults to false. A grounded jump launches upward at
6.5 metres per second, or 8.5 during a directional run, with the tangent direction retained at up to 4 metres
per second when walking or 6 metres per second when running,
unless a collision deflects it. Gravity is 9.81 metres per second squared; the unobstructed apex is
about 2.15 metres normally or 3.68 metres during a run. The stronger running launch also
increases airtime and horizontal range; holding Shift without a direction does not boost the jump.
An airborne jump cannot restart or reverse the flight.
Flight uses substeps of at most 16 ms and the same replay-safe row transaction.

`run` is an optional boolean, defaulting to false for existing clients. Walking
tangent speed is 4 metres per second; running is 6. Changing `run` in midair
does not accelerate or reverse the retained launch velocity. Server elapsed time is capped at
0.25 seconds per update, so reconnecting cannot grant a teleport. A player-row
lock atomically persists coordinates, sequence and timestamp. Duplicate or
older requests return the current position with `moving: false`, without
moving again. Login returns the persisted position and sequence.

Dry terrain and height come from the stored map and its deterministic
environment. Oceans, lakes, rivers, nonpositive terrain and slopes above 70%
block walking. Seed-derived trees, rocks, bushes and plants also block movement;
decorative grass, flowers and mushrooms do not. Parry's swept-circle query checks the entire step,
using a 0.35-metre player footprint, so endpoints cannot tunnel through objects.
An already-overlapping player may move out of a footprint, but not deeper into it.
A blocked or zero direction still advances the accepted sequence
and timestamp. Terrain generation is cached with bounded memory and performed
outside the async request worker. Parry3D sweeps the entire capsule, including
descending motion, against original triangle geometry. Contact normals slide
motion along walls; only surfaces within 35 degrees of the local horizontal
provide standing support. A one-millimetre clearance prevents overlapping landings,
and bounded contact recovery frees previously overlapping saved positions on the
next accepted movement update without a database reset. The player
can stand, walk or jump from an elevated support and falls after walking off
its edge. Decorative groundcover stays passable. Swimming and general rigid-body
physics are not implemented.

`GET /world/objects?x=<metres>&y=<metres>&z=<metres>` returns version 1, the
`heightmap_sha256`, exact string `seed`, and at most 512 nearby `objects`, ordered
nearest-first. Coordinates must be finite and within eight kilometres of the
planet radius; unknown query fields are rejected. Each descriptor contains a
stable `id`, model ID, three-component `position` in metres, `scale_m`, `yaw` in
radians, `collision_radius_m` and numeric `biome`. The embedded
[`etc/world_objects.json`](etc/world_objects.json) supplies model parameters;
placements are regenerated from the seed and grid cell, including cube-face
seams. Walking uses the same generator and footprints as the region API. No
generated planet dump or database migration is needed. Clients must bundle
matching model IDs and wait for these descriptors before enabling walking.

The catalog includes all 161 natural Nature Kit variants. Their original GLB
geometry determines footprints, with lower-trunk measurements for trees and
zero radii for small decorative groundcover. Family-normalized weights, biome
restrictions and slope checks select the variants; tests verify that every
Nature model is actually reachable by authoritative generation. Modular
structures, harvestable plants and general rigid-body physics remain separate work.

[`etc/world_colliders.json`](etc/world_colliders.json) embeds original triangle
geometry exported by the client's `tools/export-world-colliders.gd`. Regenerate
it after changing models, then rebuild the server; queries reuse cached meshes
in object-local coordinates and respect each descriptor's scale, yaw and radial
orientation. This derived geometry remains CC0, from Kenney's
[Nature Kit](https://kenney.nl/assets/nature-kit),
[Mini Forest](https://kenney.nl/assets/mini-forest),
[Platformer Kit](https://kenney.nl/assets/platformer-kit) and
[Survival Kit](https://kenney.nl/assets/survival-kit).

Usernames use 3-32 ASCII letters, digits, `_` or `-` and are case-insensitively
unique among living characters within a world. The `password` field is required
but may be empty; nonempty passwords use 8-128 UTF-8 bytes. All passwords, including
empty ones, are hashed with Argon2id. An empty password allows anyone knowing the
nickname to sign in, and is not a fallback for an incorrect nonempty password.
Sessions use 32 cryptographically random bytes, are stored only as
SHA-256 hashes, expire after one day and can be revoked. Responses use
`Cache-Control: no-store`; bodies are limited to 2048 bytes and password work is
bounded. Duplicate names return 409, bad credentials/expired sessions 401,
invalid formats/characters 400, unknown JSON fields 422 and busy authentication
429. Use TLS and deployment-level authentication rate limiting before exposing
these endpoints publicly; CORS or the two-job hashing limit are not rate limiting.

## Survival And Permanent Memorials

Migration `0015_gold_in_inventory.sql` moves gold from a balance column into the `gold` inventory item (exact balances preserved, verified inside the migration).

Migrations 0005-0008 add a permanent character UUID, inventories, starvation,
graves and immutable obituary statistics. Each new character receives four
food stacks and 100 inventory slots; gold coins are an inventory item (`gold`, 10,000 per slot, several slots allowed) and 100 coins are granted with the other starter items. Capacity grows
by 10 per level after level one, plus 20 per bag and 50 per suitcase.
Apple, bread, cheese and carrot provide 95, 265, 113 and 25 kcal respectively.
`POST /players/me/eat` accepts only an owned `item_id`, consumes one item and
returns the authoritative profile. Seven real days without eating cause death,
including while offline. A five-second worker and authenticated actions settle it.
Other death causes have server-only damage hooks, not client damage endpoints.

Migration `0012_player_exertion.sql` persists fractional reserve changes and server
timestamps without resetting existing stats. After ten seconds of accumulated
confirmed movement, walking consumes 0.1 stamina per second and running 0.5.
After five seconds without movement, stamina recovers at 0.5 per second and the
movement grace period resets. Blocked movement and repeated sequences do not
consume activity reserves. Zero stamina disables running, including its boosted
jump, and further movement costs 0.05 health per second. Movement replies include
the current authoritative `stats` for the HUD.

Water costs 0.01 per real second while the character is present (the client polls
`/events` or the character moves within the last 120 seconds), plus 0.015 per walking
second or 0.09 per running second. Zero water costs 0.2 health per real second.
The displayed integer zero is the threshold for exhaustion and dehydration;
fractional remainders preserve rates between updates. Health reaching zero causes
permanent death. Eating while already at full displayed food restores five health,
capped at 100; eating while hungry only restores food. A sleeping (disconnected)
character loses no water and takes no dehydration damage; hunger still counts offline
(seven real days without eating are fatal). Food also restores water (`item_types.water`,
migration `0031_drinking.sql`: apple and pear 8, carrot 5, coconut 25, bread and cheese 1);
inventory items report it as `water`. `POST /players/me/drink` restores 25 water when a
settlement fountain or fresh water (lake or river cell of the environment map) is within
6 metres of the character; sea water is refused as salty (`409`).

Death atomically transfers possessions into one grave and revokes all sessions.
The old character can never sign in or respawn. A new life requires registration
with a new UUID and later birth date. A deceased character's nickname may be reused;
login then authenticates the new living character, not its deceased predecessor.
There is no account/character separation yet.
Grave appearance uses lifetime gold: a headstone below 1,000, a monument from
1,000 through 1,000,000, and a mausoleum only above 1,000,000. Positive gold
receipts accumulate even after spending. Existing characters' historical
receipts cannot be recovered; migration establishes at least their balance
and the initial 100 gold as a baseline.

The obituary contains `name`, `born_at`, `lived_days`, `lifetime_gold` and `friends_count`.
Numeric values are exact decimal strings. Days are complete real days since
character creation; friends are database friendship relations at death.
Friendship management is not exposed through an API yet. Death-time values
never change when friends disappear or someone takes the grave's possessions.
Wrong passwords disclose no obituary. Login replies and profiles are not cached.

`GET /graves/{id}` returns the obituary and remaining possessions;
`POST /graves/{id}/loot` accepts `item_id` and a positive decimal-string
`quantity`. Both require a living character and authoritative positions within
three units in the same world. Missing positions deny access. There is no
global grave listing, remote retrieval, or client-controlled position setter.
Loot is transactional and enforces capacity, stack limits and overflow checks.
Empty graves remain. Database triggers prevent deletion or overwriting memorial
facts; backups and restricted administrative permissions are still essential.
`GET /world/objects?x=...&y=...&z=...` includes nearby memorial descriptors;
`GET /world/memorials` with the same coordinates provides lightweight refreshes.
Both expose at most 128 graves within 700 metres along the planet surface, with
exact death positions and server-selected kinds, not possessions or account data.
The client renders the bundled Kenney memorials at these positions. Physical
grave collisions and click-to-inspect interaction remain unimplemented.

## Tests

```sh
cargo test --locked
DATABASE_URL='postgresql:///postgres?host=/var/run/postgresql' \
  ISHTARIA_TEST_HEIGHTMAP="$PWD/../ishtaria-worldgen/examples/planet-seed42-256.pgm" \
  cargo test --locked -- --include-ignored
```

Database tests require `CREATEDB` and use isolated databases managed by SQLx, never the live world database. They cover migrations, idempotent import, overwrite protection, transaction rollback, restarting without a source file, byte-exact HTTP download, all face indices, invalid coordinates, forbidden writes, and database health failures. They also cover one-time gold grants, persistent changed balances, registration, login, profile access, logout/expiry, character validation and character restoration after initialization. Without `DATABASE_URL`, normal `cargo test` runs the parser tests and skips PostgreSQL integration tests.

Survival tests also cover eating, concurrent loot, permanent death, session
revocation, UUIDs, wealth thresholds, obituary snapshots and empty memorials.
Movement tests cover speed limits, radial/water rejection, concurrent duplicate
requests, replay protection, invalid inputs and position persistence after login.
Set `ISHTARIA_TEST_CLIENT=1` on the isolated test command to additionally run
Godot registration, eating and deceased-login obituary checks over actual HTTP.

## Installation

Debian / Ubuntu (x86-64) only:

```sh
echo "deb http://repo.vitexsoftware.com $(lsb_release -sc) main" | sudo tee /etc/apt/sources.list.d/vitexsoftware.list
sudo wget -O /etc/apt/trusted.gpg.d/vitexsoftware.gpg http://repo.vitexsoftware.com/keyring.gpg
sudo apt update
sudo apt install ishtaria-server ishtaria-content
```

```sh
sudoedit /etc/ishtaria/server.toml        # server_name is permanent – choose carefully
sudo ishtaria-server-init                 # generate a planet with ishtaria-worldgen and import it once
sudo systemctl enable --now ishtaria-server
```

The package creates the PostgreSQL role and database `ishtaria` if PostgreSQL is
running during installation. Day-to-day administration (maps, bans, portals,
scheduled shutdown) is done with `ishtaria-admin`; migration `0013_admin.sql`
holds its tables. `GET /world` carries `messages[]`; entries with `kind: "system"`
(for example `code: "shutdown"` with `seconds_left`) must be shown by clients and
cannot be dismissed. Banned players get `403 account banned` on login and lose
their session.

## Building

```sh
cargo build --release
dpkg-buildpackage -us -uc -b
```

To develop against a local checkout of `ishtaria-core`, add to `.cargo/config.toml` (not committed):

```toml
[patch."https://github.com/VitexSoftware/ishtaria-core"]
ishtaria-core = { path = "../ishtaria-core" }
```

License: AGPL-3.0-only – anyone may run a world; modified servers offered over the network must publish their source.

## Part of Ishtaria

Ishtaria is an open-source, persistent, federated virtual planet of Earth size.
Documentation: https://vitexsoftware.github.io/ishtaria-docs/ · All repositories: https://github.com/VitexSoftware?q=ishtaria

## Federation: portals and share links

A player builds a portal alone; a finished portal can be linked with a finished portal of another world
by pasting its **share link**. Set `public_url` (and optionally `[federation] policy = "closed" | "approve" |
"open"`, default `approve`) in `server.toml` to let the world link portals. Migration `0014_federation.sql`
stores the world's Ed25519 signing key and pinned peer keys, `0030_portal_links.sql` the standalone portals
and broken links; the key lives in the database, so protect and back it up like the rest of the world.

| Endpoint | Purpose |
|---|---|
| `GET /.well-known/ishtaria/server.json` | Published identity: world name, API URL, public key, policy |
| `POST /portals/build`, `GET /portals/mine`, `GET /portals/mine/{id}` | Start a portal where the player stands / list / progress; authenticated |
| `POST /portals/mine/{id}/contribute` | Deliver materials (`etc/portal.json`); the last delivery finishes the portal |
| `GET /portals/mine/{id}/link` | The share link of a finished, unlinked portal |
| `POST /portals/mine/{id}/connect` | Paste another world's link: the server verifies the peer (reachable, signed, the portal stands there and is finished), then links both ends |
| `DELETE /portals/mine/{id}/link`, `DELETE /portals/mine/{id}` | Break the link (the portal is finished again) / close the portal (ruin, link broken) |
| `GET /federation/portals/{id}` | Public: does this portal stand here, and is it finished |
| `POST /federation/portals/link`, `POST /federation/portals/unlink` | Server-to-server signed messages (4 KiB limit) |
| `GET /world/portals` | Portals near a point, for rendering |

A portal has exactly one counterpart. Outbound requests to peers use public addresses only, no redirects, small
bodies and a five-second timeout; set `allow_private_peers = true` only on development networks. With policy
`approve` a link stays `pending` until the operator approves it in `ishtaria-admin` (F4, *Open*). Message formats are in `ishtaria-protocol`; see the documentation page *Portals and share links*.

## Land leases (real money)

Off by default; enable it in `server.toml`:

```toml
[monetization]
enabled = true
currency = "CZK"
tile_price_minor = 1000     # per tile and month, in minor units (10.00)
grace_days = 7              # unpaid rent keeps its exclusive right this long
max_tiles = 400
```

`GET /land/prices`, `GET/POST /land/leases` (a rectangle of map tiles near the player, at most 40 tiles a
side), `POST /land/leases/{id}/renew`, `GET /land/leased?x&y&z`. An order is confirmed by the payment
provider's signed callback `POST /payments/webhook` (`X-Ishtaria-Signature`: hex HMAC-SHA256 of the body
with the secret in the environment variable `ISHTARIA_PAYMENT_SECRET`, at least 16 characters). The only
provider so far is `manual`: the operator, or a script, posts the signed callback. The server handles no
card data and applies a payment idempotently in one transaction. Only the tenant can place construction
sites on leased land. Migration `0020_land_leases.sql`.

## Experience, levels and the hall of fame

Every swing at a tree or a rock gives 1 experience point, crafting gives more (`xp` per recipe in
`etc/recipes.json`: 3 for chopping a log, 4 for planks, 5 for a stone block) and building a portal still
more (5 per delivered unit and 500 for a finished end). A character is at level `n` from
`25 * n * (n - 1)` points (50, 150, 300, 500 … 2250 for level 10, 9500 for level 20); the profile's `stats`
carry `level_experience` and `next_level_experience`. Each level above the first gives 10 inventory slots,
up to level 30. `GET /hall-of-fame` lists the ten best characters, living or dead, by
`score = 100 * level + 10 * days lived` with their wealth; a dead character's wealth shows as zero once
anyone has taken something from the grave (`looted`).

## Gathering and crafting

`POST /players/me/harvest` (`{"object_id": …}`) fells trees and mines rocks of the generated world;
`POST /players/me/craft` (`{"recipe": …, "count": "1"}`) and `GET /recipes` craft items. Resources are defined in
`etc/resources.json`, recipes in `etc/recipes.json`, items by migration `0016_gathering.sql`. Axe, pickaxe and
sword are inventory items every new character starts with; `POST /players/me/equip` (`{"item_id": …}`) puts a tool or weapon in hand and `DELETE /players/me/equip` takes it out (`equipment.hand` in the profile, migration `0021_equipment.sql`). Only the tool in hand works: the axe fells trees, the pickaxe mines stone and also fells trees, but needs twice as many swings. A log fills ten slots and is chopped into wood;
harvested objects are stored as changes (`world_object_state`) and grow back; a felled tree leaves a stump.

Land animals walk (their route is a pure function of id and time, sent as `wander` waypoints with `server_time_ms` in
`GET /world/objects`) and graze in pastures beside generated settlements (`<seed>:farm:<n>:<place>`).
`POST /players/me/butcher` (`{"object_id": …}`) swings the weapon in hand at an animal in reach and yields raw meat
after several swings (`etc/creatures.json`; the animal returns later, stored in `world_object_state`);
`POST /players/me/milk` lets a character drink from a cow (30 water, ten minutes per cow, `creature_milked`,
migration `0034_creatures.sql`).

**Combat (migration `0036_combat.sql`, `src/combat.rs`, `etc/combat.json`).** A dangerous animal (wolf, husky, fox, stag, bull)
may bite back after a hit it survives; the blow is cut by worn armour (`body`: `armor_leather`/`armor_golden`/`armor_metal`,
`hands`: `glove`; at most 60 %) and to a quarter by a raised shield, both wear, and a killing blow is a permanent death
(cause `creature`, reply 410 with the obituary). Armour is put on with `POST /players/me/equip` and taken off with
`DELETE /players/me/equip?slot=body|hands`; the profile's `equipment` shows `body`, `hands` and `defense`. Hunting drops hides, iron
ore is smelted to ingots (`smelt_iron_*`, pickaxe as an interim tool until crafting stations exist) and weapons, armour and
shields are crafted from ingots and hides (`etc/recipes.json`). Animals still do not chase; players cannot hurt players.

**Trading (migration `0037_trade_and_shops.sql`, `src/shops.rs`, `src/trade.rs`, `etc/shops.json`).**
An NPC tagged `shop:<id>` is a merchant; every generated town has a trader at its plaza (`etc/trader.yaml`, tag `shop:general`) and
a datadisk can tag any NPC. `GET /shops/{npc_id}` lists what the shop sells and buys (character must stand within 6 m),
`POST /shops/{npc_id}/buy|sell` (`{"item_id", "quantity"}`) exchange items for gold in one transaction. Selling pays less than buying
costs, and one character can sell at most `daily_sell_limit_gold` (400) a day to a shop (`shop_sales`), which bounds the gold the
world creates. Between players there is one open exchange at a time: `POST /trades {"with": name}` (both within 10 m),
`GET|DELETE /trades/current`, `PUT /trades/current/offer {"items": [{item_id, quantity}]}` (gold is the item `gold`; at most 10 stacks),
`POST /trades/current/accept`. Changing an offer withdraws both acceptances; when both have accepted, the server re-checks ownership,
distance and capacity and swaps everything or nothing. Used pieces (with remaining durability) cannot be traded, so trading cannot
make a worn tool new. Exchanges are deleted when they end and expire after five minutes; the partner is told by a `trade` event.

**Building, farming, cooking, fishing (migration `0038_building_farming_fishing.sql`, `src/placing.rs`, `src/fishing.rs`,
`etc/placeables.json`).** `POST /players/me/place` (`{"item_id", "x", "y", "z", "yaw"}`, metres from the planet's centre, within 4 m of the
character, on dry land) puts a campfire, bedroll, workbench, anvil, tent or fence into the world; `POST /players/me/plant` does the
same with a seed (`seed_carrot|corn|cabbage|pumpkin`). Things are rows of `placed_objects`, `GET /world/placed?x&y&z` lists them within
150 m (public, like `/world/objects`; crops carry `growth` 0..1 and `ripe`, computed from the clock so offline time counts).
`POST /placed/{id}/pickup` and `/harvest` are for the owner within reach (a ripe crop gives its produce and one seed, an unripe one
gives the seed back). Recipes may name a `station` (`campfire`, `workbench`, `anvil`): a placed station of that kind within 4 m of
the character, whoever owns it. `POST /players/me/fish` needs a fishing rod in hand and water within 6 m and catches a fish about
every second cast. Limits: 40 things and 60 crops per character; nothing on water; spacing between things. Not implemented: protection
of leased land, collision with placed things, storage chests, sleeping in a bed, soil quality or seasons for crops.

**Magic (migration `0039_magic.sql`, `src/magic.rs`, `etc/spells.json`, ADR 0010).** Mana is a reserve of 0-100 that regenerates by the clock
(0.4/s, stored with `mana_updated_at`, so offline time counts); it is part of `stats` (`mana`, `mana_max`). Spells are data (cost, cooldown,
minimum level and one of the effects `heal`, `refresh`, `ward`, `bolt`) and are learned from scrolls for good: `POST /players/me/learn`
(`{"item_id": "scroll_..."}`), `GET /players/me/spells` (known spells with `ready_in_ms`), `POST /players/me/cast` (`{"spell", "object_id"?}`).
A bolt hits an animal in range like a weapon swing (drops, respawn, XP as in hunting) and avoids the bite of animals that would strike back in melee;
a ward lets only half of a creature's bite through for a minute. Scrolls are crafted at a workbench from a rare quartz crystal and are not sold.
No spell targets other players.

**Protection and observation (`src/guard.rs`, `[limits]` in `server.toml`).** A middleware limits registration and login per address
(`auth_per_minute`, shared with logins) and all requests per address (token bucket, `requests_per_second` and `burst`), times every
request out after 30 s, writes an access log to standard error (never query strings or tokens) and counts requests for
`GET /metrics` (Prometheus text, loopback or `metrics_token` only). All request bodies are limited to 64 KiB unless a route sets less.
Hashing passwords is limited to two at a time; a burst of logins waits up to 3 s for its turn before it is refused with 429.
`tools/loadtest.py` is a small load test (see the operations documentation for a measurement). The limits live in memory and are per
server process; a router without the guard (every test) is not limited.

## Story datadisks

Graveyards generated around towns play the default track `assets/music/graveyard_midnightcem.ogg` (*Midnightcem* by Tozan, CC0;
installed to `/usr/share/ishtaria-server/music/`, override the directory with `ISHTARIA_ASSETS_DIR`; without the file they are silent).
A datadisk's own graveyard names its own music in its place.

A datadisk (directory under `/usr/share/ishtaria/datadisks/<id>/`, override with `ISHTARIA_DATADISK_DIR`) adds
places, characters, dialogue trees and quests to a world; several disks can be combined. Format and API:
[docs](https://vitexsoftware.github.io/ishtaria-docs/architecture/story.html) and
`ishtaria-protocol/schemas/datadisk.schema.json`. Disks are chosen when the world is generated: the
`ishtaria-admin` map dialog shows a checkbox per installed disk, or run
`ishtaria-server-init <seed> <size> <disk-id> ...`. `ishtaria-server --list-datadisks` lists installed disks and
`--check-datadisks a,b` checks that they can be combined. Migration `0026_story.sql` stores the selected disks
(`world_datadisks`, content hash pinned on first load), the placed sites (`story_anchors`), quest stages, flags and
once-only rewards. Endpoints: `POST /story/dialogue/start|choose`, `DELETE /story/dialogue`, `GET /story/quests`,
`GET /story/strings` (every language; the server never learns the client's language) and
`GET /story/media/<disk>/<path>` (portraits and music a loaded disk names); `GET /world/objects` carries `npcs`.
Choices are applied by the server in one transaction (gold, items, flags, stages); a stale sequence number is refused.
