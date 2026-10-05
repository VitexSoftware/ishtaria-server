//! Real-money leases of land.
//!
//! The operator of a world rents out rectangles of map tiles. Only the tenant can
//! build on rented land. Rent is paid monthly; an unpaid lease keeps its exclusive
//! right for the grace period and then lapses, after which the land is free for
//! others. Payments are orders confirmed by a signed webhook of the payment
//! provider; the server never sees card data. The whole feature is off unless the
//! operator enables it (`[monetization]` in `server.toml`).

use super::players::{self, Error};
use super::AppState;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::FromRow;

const PENDING_HOLD_MINUTES: i32 = 30;
const MAX_LEASES_PER_PLAYER: i64 = 5;
const MAX_SIDE_TILES: i32 = 40;
const NEAR_TILES: i32 = 12;
const LISTING_TILES: i32 = 60;
const MIN_SECRET_LENGTH: usize = 16;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/land/prices", get(prices))
        .route("/land/leases", get(my_leases).post(create_lease))
        .route("/land/leases/{id}/renew", post(renew_lease))
        .route("/land/leased", get(leased_near))
        .route(
            "/payments/webhook",
            post(webhook).layer(DefaultBodyLimit::max(4096)),
        )
}

#[derive(FromRow)]
struct Settings {
    enabled: bool,
    provider: String,
    currency: String,
    tile_price_minor: i64,
    model_price_minor: i64,
    avatar_price_minor: i64,
    grace_days: i32,
    max_tiles: i32,
}

fn status(code: StatusCode, message: &'static str) -> Error {
    Error::Status(code, message)
}

async fn settings(state: &AppState) -> Result<Settings, Error> {
    sqlx::query_as("SELECT enabled, provider, currency, tile_price_minor, model_price_minor, avatar_price_minor, grace_days, max_tiles FROM monetization_settings WHERE world_id = $1")
        .bind(state.world_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| status(StatusCode::SERVICE_UNAVAILABLE, "monetization unavailable"))
}

/// Operator configuration of the monetization settings.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct MonetizationConfig {
    enabled: Option<bool>,
    currency: Option<String>,
    tile_price_minor: Option<i64>,
    model_price_minor: Option<i64>,
    avatar_price_minor: Option<i64>,
    grace_days: Option<i32>,
    max_tiles: Option<i32>,
}

pub(super) async fn ensure_settings(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    world_id: i64,
    config: Option<&MonetizationConfig>,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO monetization_settings (world_id) VALUES ($1) ON CONFLICT (world_id) DO NOTHING")
        .bind(world_id)
        .execute(&mut **transaction)
        .await?;
    if let Some(config) = config {
        if let Some(currency) = &config.currency {
            anyhow::ensure!(
                currency.len() == 3 && currency.bytes().all(|byte| byte.is_ascii_uppercase()),
                "monetization currency must be a three-letter ISO code such as CZK"
            );
        }
        sqlx::query("UPDATE monetization_settings SET enabled = coalesce($2, enabled), currency = coalesce($3, currency), tile_price_minor = coalesce($4, tile_price_minor), model_price_minor = coalesce($5, model_price_minor), avatar_price_minor = coalesce($6, avatar_price_minor), grace_days = coalesce($7, grace_days), max_tiles = coalesce($8, max_tiles), updated_at = now() WHERE world_id = $1")
            .bind(world_id).bind(config.enabled).bind(&config.currency).bind(config.tile_price_minor)
            .bind(config.model_price_minor).bind(config.avatar_price_minor).bind(config.grace_days).bind(config.max_tiles)
            .execute(&mut **transaction).await?;
    }
    Ok(())
}

#[derive(Serialize)]
struct Prices {
    enabled: bool,
    currency: String,
    tile_price_minor: String,
    model_price_minor: String,
    avatar_price_minor: String,
    grace_days: i32,
    max_tiles: i32,
}

async fn prices(State(state): State<AppState>) -> Result<Json<Prices>, Error> {
    let settings = settings(&state).await?;
    Ok(Json(Prices {
        enabled: settings.enabled,
        currency: settings.currency,
        tile_price_minor: settings.tile_price_minor.to_string(),
        model_price_minor: settings.model_price_minor.to_string(),
        avatar_price_minor: settings.avatar_price_minor.to_string(),
        grace_days: settings.grace_days,
        max_tiles: settings.max_tiles,
    }))
}

/// State of a lease: `pending` (waiting for the first payment, held for a while), `expired`
/// (never paid), `active`, `grace` (unpaid but still exclusive), `lapsed` or `released`.
fn state_sql(grace_days: i32) -> String {
    format!("CASE WHEN released_at IS NOT NULL THEN 'released' WHEN paid_until IS NULL THEN CASE WHEN created_at > now() - make_interval(mins => {PENDING_HOLD_MINUTES}) THEN 'pending' ELSE 'expired' END WHEN now() < paid_until THEN 'active' WHEN now() < paid_until + make_interval(days => {grace_days}) THEN 'grace' ELSE 'lapsed' END")
}

#[derive(Serialize, FromRow)]
struct Lease {
    id: String,
    state: String,
    face: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    tiles: i64,
    tile_price_minor: String,
    currency: String,
    paid_until: Option<i64>,
}

async fn my_leases(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<Lease>>, Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let settings = settings(&state).await?;
    let state_sql = state_sql(settings.grace_days);
    Ok(Json(
        sqlx::query_as(&format!("SELECT id::text, ({state_sql}) AS state, face, x0, y0, x1, y1, ((x1 - x0 + 1)::bigint * (y1 - y0 + 1)) AS tiles, tile_price_minor::text, currency, extract(epoch FROM paid_until)::bigint AS paid_until FROM land_leases WHERE player_id = $1 AND world_id = $2 AND released_at IS NULL ORDER BY created_at DESC LIMIT 50"))
            .bind(player_id).bind(state.world_id).fetch_all(&state.pool).await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewLease {
    face: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

#[derive(Serialize)]
struct Checkout {
    provider: String,
    /// What the customer quotes or the provider's checkout carries: the order id.
    reference: String,
}

#[derive(Serialize)]
struct Ordered {
    lease_id: String,
    order_id: String,
    amount_minor: String,
    currency: String,
    checkout: Checkout,
}

async fn create_lease(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<NewLease>,
) -> Result<(StatusCode, Json<Ordered>), Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let settings = settings(&state).await?;
    if !settings.enabled {
        return Err(status(StatusCode::FORBIDDEN, "land leases are not offered"));
    }
    let grid = super::movement::OBJECT_GRID;
    if !(0..=5).contains(&input.face)
        || input.x0 < 0
        || input.y0 < 0
        || input.x1 < input.x0
        || input.y1 < input.y0
        || input.x1 >= grid
        || input.y1 >= grid
        || input.x1 - input.x0 >= MAX_SIDE_TILES
        || input.y1 - input.y0 >= MAX_SIDE_TILES
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid parcel"));
    }
    let tiles = i64::from(input.x1 - input.x0 + 1) * i64::from(input.y1 - input.y0 + 1);
    if tiles > i64::from(settings.max_tiles) {
        return Err(status(StatusCode::BAD_REQUEST, "the parcel is too large"));
    }
    let amount = tiles
        .checked_mul(settings.tile_price_minor)
        .ok_or_else(|| status(StatusCode::BAD_REQUEST, "invalid parcel"))?;
    let mut transaction = state.pool.begin().await?;
    let position: Option<(f64, f64, f64)> = sqlx::query_as("SELECT position_x, position_y, position_z FROM players WHERE id = $1 AND position_x IS NOT NULL FOR UPDATE")
        .bind(player_id).fetch_optional(&mut *transaction).await?;
    let (x, y, z) =
        position.ok_or_else(|| status(StatusCode::CONFLICT, "player position unavailable"))?;
    let (face, column, row) = super::movement::tile_of([x, y, z]);
    let nearest = |value: i32, low: i32, high: i32| (value - value.clamp(low, high)).abs();
    if face != input.face
        || nearest(column, input.x0, input.x1) > NEAR_TILES
        || nearest(row, input.y0, input.y1) > NEAR_TILES
    {
        return Err(status(
            StatusCode::FORBIDDEN,
            "the parcel is too far from the player",
        ));
    }
    // One lease at a time per world is decided here, so parcels never overlap.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('land_leases'), $1::integer)")
        .bind(state.world_id)
        .execute(&mut *transaction)
        .await?;
    let state_sql = state_sql(settings.grace_days);
    let own: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM land_leases WHERE player_id = $1 AND released_at IS NULL AND ({state_sql}) IN ('pending', 'active', 'grace')"))
        .bind(player_id).fetch_one(&mut *transaction).await?;
    if own >= MAX_LEASES_PER_PLAYER {
        return Err(status(StatusCode::TOO_MANY_REQUESTS, "too many leases"));
    }
    let taken: bool = sqlx::query_scalar(&format!("SELECT EXISTS (SELECT 1 FROM land_leases WHERE world_id = $1 AND face = $2 AND released_at IS NULL AND x0 <= $5 AND x1 >= $3 AND y0 <= $6 AND y1 >= $4 AND ({state_sql}) IN ('pending', 'active', 'grace'))"))
        .bind(state.world_id).bind(input.face).bind(input.x0).bind(input.y0).bind(input.x1).bind(input.y1)
        .fetch_one(&mut *transaction).await?;
    if taken {
        return Err(status(StatusCode::CONFLICT, "the land is already leased"));
    }
    let lease_id: String = sqlx::query_scalar("INSERT INTO land_leases (world_id, player_id, face, x0, y0, x1, y1, tile_price_minor, currency) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING id::text")
        .bind(state.world_id).bind(player_id).bind(input.face).bind(input.x0).bind(input.y0).bind(input.x1).bind(input.y1)
        .bind(settings.tile_price_minor).bind(&settings.currency)
        .fetch_one(&mut *transaction).await?;
    let order_id: String = sqlx::query_scalar("INSERT INTO payment_orders (world_id, player_id, kind, lease_id, amount_minor, currency, provider) VALUES ($1, $2, 'lease_new', $3::uuid, $4, $5, $6) RETURNING id::text")
        .bind(state.world_id).bind(player_id).bind(&lease_id).bind(amount).bind(&settings.currency).bind(&settings.provider)
        .fetch_one(&mut *transaction).await?;
    transaction.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(Ordered {
            lease_id,
            checkout: Checkout {
                provider: settings.provider,
                reference: order_id.clone(),
            },
            order_id,
            amount_minor: amount.to_string(),
            currency: settings.currency,
        }),
    ))
}

/// Orders the next month of a lease. Paying it extends `paid_until` by one month from its
/// current end, so billing periods follow each other without gaps or overlap.
async fn renew_lease(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Ordered>), Error> {
    let player_id = players::player_id(&state, &players::token_hash(&headers)?).await?;
    let settings = settings(&state).await?;
    if !settings.enabled {
        return Err(status(StatusCode::FORBIDDEN, "land leases are not offered"));
    }
    if id.len() != 36 {
        return Err(status(StatusCode::NOT_FOUND, "lease not found"));
    }
    let state_sql = state_sql(settings.grace_days);
    let mut transaction = state.pool.begin().await?;
    let lease: Option<(String, i64, String, i64)> = sqlx::query_as(&format!("SELECT ({state_sql}) AS state, ((x1 - x0 + 1)::bigint * (y1 - y0 + 1) * tile_price_minor), currency, 0::bigint FROM land_leases WHERE id = $1::uuid AND world_id = $2 AND player_id = $3 FOR UPDATE"))
        .bind(&id).bind(state.world_id).bind(player_id).fetch_optional(&mut *transaction).await?;
    let Some((lease_state, amount, currency, _)) = lease else {
        return Err(status(StatusCode::NOT_FOUND, "lease not found"));
    };
    if !matches!(lease_state.as_str(), "active" | "grace") {
        return Err(status(StatusCode::CONFLICT, "the lease cannot be renewed"));
    }
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM payment_orders WHERE lease_id = $1::uuid AND status = 'pending' AND kind = 'lease_renew'")
        .bind(&id).fetch_one(&mut *transaction).await?;
    if open >= 3 {
        return Err(status(
            StatusCode::TOO_MANY_REQUESTS,
            "too many unpaid orders",
        ));
    }
    let order_id: String = sqlx::query_scalar("INSERT INTO payment_orders (world_id, player_id, kind, lease_id, amount_minor, currency, provider) VALUES ($1, $2, 'lease_renew', $3::uuid, $4, $5, $6) RETURNING id::text")
        .bind(state.world_id).bind(player_id).bind(&id).bind(amount).bind(&currency).bind(&settings.provider)
        .fetch_one(&mut *transaction).await?;
    transaction.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(Ordered {
            lease_id: id,
            checkout: Checkout {
                provider: settings.provider,
                reference: order_id.clone(),
            },
            order_id,
            amount_minor: amount.to_string(),
            currency,
        }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Region {
    x: f64,
    y: f64,
    z: f64,
}

#[derive(Serialize, FromRow)]
struct Leased {
    tenant: String,
    state: String,
    face: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

/// Leased parcels around a point, for drawing their borders.
async fn leased_near(
    State(state): State<AppState>,
    Query(region): Query<Region>,
) -> Result<Json<Vec<Leased>>, Error> {
    if [region.x, region.y, region.z]
        .iter()
        .any(|value| !value.is_finite())
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid region"));
    }
    let settings = settings(&state).await?;
    let (face, column, row) = super::movement::tile_of([region.x, region.y, region.z]);
    let state_sql = state_sql(settings.grace_days);
    Ok(Json(
        sqlx::query_as(&format!("SELECT username AS tenant, state, face, x0, y0, x1, y1 FROM (SELECT players.username, ({state_sql}) AS state, land_leases.face, x0, y0, x1, y1 FROM land_leases JOIN players ON players.id = land_leases.player_id WHERE land_leases.world_id = $1 AND land_leases.face = $2 AND released_at IS NULL AND x0 <= $3 + {LISTING_TILES} AND x1 >= $3 - {LISTING_TILES} AND y0 <= $4 + {LISTING_TILES} AND y1 >= $4 - {LISTING_TILES}) leases WHERE state IN ('active', 'grace') ORDER BY x0, y0 LIMIT 64"))
            .bind(state.world_id).bind(face).bind(column).bind(row).fetch_all(&state.pool).await?,
    ))
}

/// Refuses building on land that another player holds. Free land and the tenant's own
/// land are open; a lease keeps its exclusive right through the grace period.
pub(super) async fn require_tenant(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    world_id: i64,
    player_id: i64,
    position: [f64; 3],
) -> Result<(), Error> {
    let grace: i32 =
        sqlx::query_scalar("SELECT grace_days FROM monetization_settings WHERE world_id = $1")
            .bind(world_id)
            .fetch_optional(&mut **transaction)
            .await?
            .unwrap_or(7);
    let (face, column, row) = super::movement::tile_of(position);
    let state_sql = state_sql(grace);
    let foreign: bool = sqlx::query_scalar(&format!("SELECT EXISTS (SELECT 1 FROM land_leases WHERE world_id = $1 AND face = $2 AND x0 <= $3 AND x1 >= $3 AND y0 <= $4 AND y1 >= $4 AND player_id <> $5 AND released_at IS NULL AND ({state_sql}) IN ('active', 'grace'))"))
        .bind(world_id).bind(face).bind(column).bind(row).bind(player_id)
        .fetch_one(&mut **transaction).await?;
    if foreign {
        return Err(status(
            StatusCode::FORBIDDEN,
            "the land is leased by another player",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    order_id: String,
    status: String,
    reference: String,
}

#[derive(Serialize)]
struct Confirmed {
    order: String,
}

fn payment_secret() -> Option<Vec<u8>> {
    std::env::var("ISHTARIA_PAYMENT_SECRET")
        .ok()
        .filter(|secret| secret.len() >= MIN_SECRET_LENGTH)
        .map(String::into_bytes)
}

/// Verifies `X-Ishtaria-Signature`: the hex HMAC-SHA256 of the body with the shared secret.
fn signature_valid(secret: &[u8], headers: &HeaderMap, body: &[u8]) -> bool {
    let Some(signature) = headers
        .get("x-ishtaria-signature")
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.len() == 64)
        .and_then(|value| {
            (0..32)
                .map(|index| u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok())
                .collect::<Option<Vec<u8>>>()
        })
    else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

/// Payment provider callback. Applying the result is idempotent and atomic.
async fn webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Confirmed>, Error> {
    let settings = settings(&state).await?;
    if !settings.enabled {
        return Err(status(StatusCode::FORBIDDEN, "payments are not accepted"));
    }
    let secret = payment_secret().ok_or_else(|| {
        status(
            StatusCode::SERVICE_UNAVAILABLE,
            "payments are not configured",
        )
    })?;
    if !signature_valid(&secret, &headers, &body) {
        return Err(status(StatusCode::UNAUTHORIZED, "invalid signature"));
    }
    let event: Event = serde_json::from_slice(&body)
        .map_err(|_| status(StatusCode::BAD_REQUEST, "invalid event"))?;
    if event.order_id.len() != 36
        || event.reference.is_empty()
        || event.reference.len() > 200
        || !["paid", "failed", "refunded"].contains(&event.status.as_str())
    {
        return Err(status(StatusCode::BAD_REQUEST, "invalid event"));
    }
    let mut transaction = state.pool.begin().await?;
    let order: Option<(String, String, Option<String>)> = sqlx::query_as("SELECT status, kind, lease_id::text FROM payment_orders WHERE id = $1::uuid AND world_id = $2 FOR UPDATE")
        .bind(&event.order_id).bind(state.world_id).fetch_optional(&mut *transaction).await?;
    let Some((current, kind, lease)) = order else {
        return Err(status(StatusCode::NOT_FOUND, "order not found"));
    };
    let target = match event.status.as_str() {
        "paid" => "paid",
        "failed" => "failed",
        _ => "refunded",
    };
    if current == target {
        return Ok(Json(Confirmed { order: current }));
    }
    let allowed = matches!(
        (current.as_str(), target),
        ("pending", "paid") | ("pending", "failed") | ("paid", "refunded")
    );
    if !allowed {
        return Err(status(
            StatusCode::CONFLICT,
            "the order cannot change this way",
        ));
    }
    let updated = sqlx::query("UPDATE payment_orders SET status = $2, provider_ref = coalesce(provider_ref, $3), updated_at = now() WHERE id = $1::uuid AND (provider_ref IS NULL OR provider_ref = $3)")
        .bind(&event.order_id).bind(target).bind(&event.reference)
        .execute(&mut *transaction).await;
    match updated {
        Ok(result) if result.rows_affected() == 1 => {}
        Ok(_) => {
            return Err(status(
                StatusCode::CONFLICT,
                "the reference belongs to another payment",
            ))
        }
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            return Err(status(
                StatusCode::CONFLICT,
                "the reference belongs to another payment",
            ));
        }
        Err(error) => return Err(error.into()),
    }
    if let Some(lease) = lease {
        match (kind.as_str(), target) {
            ("lease_new", "paid") => {
                sqlx::query("UPDATE land_leases SET paid_until = now() + interval '1 month' WHERE id = $1::uuid AND paid_until IS NULL AND released_at IS NULL")
                    .bind(&lease).execute(&mut *transaction).await?;
            }
            ("lease_renew", "paid") => {
                sqlx::query("UPDATE land_leases SET paid_until = paid_until + interval '1 month' WHERE id = $1::uuid AND paid_until IS NOT NULL AND released_at IS NULL")
                    .bind(&lease).execute(&mut *transaction).await?;
            }
            ("lease_new", "failed") | (_, "refunded") => {
                sqlx::query("UPDATE land_leases SET released_at = now() WHERE id = $1::uuid AND released_at IS NULL")
                    .bind(&lease).execute(&mut *transaction).await?;
            }
            _ => {}
        }
    }
    transaction.commit().await?;
    Ok(Json(Confirmed {
        order: target.to_owned(),
    }))
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn signatures_need_the_secret_and_the_exact_body() {
        let secret = b"0123456789abcdef-secret";
        let body = br#"{"order_id":"x"}"#;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
        mac.update(body);
        let good: String = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let headers = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("x-ishtaria-signature", value.parse().unwrap());
            headers
        };
        assert!(signature_valid(secret, &headers(&good), body));
        assert!(!signature_valid(secret, &headers(&good), b"{}"));
        assert!(!signature_valid(
            b"another-secret-value",
            &headers(&good),
            body
        ));
        assert!(!signature_valid(secret, &headers(&good[..62]), body));
        assert!(!signature_valid(secret, &headers(&"z".repeat(64)), body));
        assert!(!signature_valid(secret, &HeaderMap::new(), body));
    }
}
