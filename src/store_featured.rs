// store_featured.rs — the storefront's "Featured & Recommended" picks.
//
// The desktop client personalizes its hero locally, from the playtime it has in
// its own library.json. The website has no such local state, so the same idea is
// computed here from `game_stats` (per-user playtime) and returned ready to
// render. The scoring deliberately mirrors the client's `recommend.ts` — same
// franchise/genre/platform weights, same rating tiebreak — so a user sees a
// consistent shortlist in both places.
//
// Assembled via include! at crate-root scope like the other modules.
//
// SECURITY: like the rest of store_api, this requires a storefront session and
// returns only catalog metadata — never content_path, launch or user rows.

use rand::seq::SliceRandom;
use std::collections::HashMap;

// A franchise match is the strongest signal (you played Metroid, here is another
// Metroid), then genre, then the platform you spend the most time on.
const FEATURED_FRANCHISE_WEIGHT: f64 = 3.0;
const FEATURED_GENRE_WEIGHT: f64 = 2.0;
const FEATURED_PLATFORM_WEIGHT: f64 = 1.0;
// Lets a good game edge out a mediocre one at equal affinity, without ever
// outweighing an affinity match.
const FEATURED_RATING_WEIGHT: f64 = 0.5;
/// How many picks the hero rotates through.
const FEATURED_LIMIT: usize = 6;
/// Platform shown in the cold-start hero, matched case-insensitively against
/// `games.platform` (stored as "PC" alongside "NES", "SNES", "N64", …).
const FEATURED_COLD_START_PLATFORM: &str = "PC";

/// Attribute → share of tracked playtime, scaled so the strongest is 1.0.
type TasteWeights = HashMap<String, f64>;

#[derive(Debug, Default)]
struct Taste {
    platforms: TasteWeights,
    genres: TasteWeights,
    franchises: TasteWeights,
    /// Total tracked seconds behind the profile; 0 means "nothing played yet",
    /// which is what makes the response non-personalized.
    total_seconds: i64,
}

/// Case-insensitive key so "Action RPG" and "action rpg" pool together.
fn taste_key(value: &str) -> String {
    value.trim().to_lowercase()
}

fn taste_add(into: &mut TasteWeights, value: &str, seconds: i64) {
    let k = taste_key(value);
    if k.is_empty() {
        return;
    }
    *into.entry(k).or_insert(0.0) += seconds as f64;
}

/// Scale a weight map so its largest entry is 1. An empty map stays empty.
fn taste_normalize(raw: &mut TasteWeights) {
    let max = raw.values().cloned().fold(0.0_f64, f64::max);
    if max <= 0.0 {
        raw.clear();
        return;
    }
    for v in raw.values_mut() {
        *v /= max;
    }
}

/// Build a taste profile from the caller's playtime. `played` maps game id →
/// seconds; games absent from it (or at zero) contribute nothing.
fn build_taste(games: &[Game], played: &HashMap<String, i64>) -> Taste {
    let mut t = Taste::default();
    for g in games {
        let secs = played.get(&g.id).copied().unwrap_or(0);
        if secs <= 0 {
            continue;
        }
        t.total_seconds += secs;
        taste_add(&mut t.platforms, &g.platform, secs);
        taste_add(&mut t.franchises, &g.franchise, secs);
        for tag in split_genres(&g.genres) {
            taste_add(&mut t.genres, &tag, secs);
        }
    }
    taste_normalize(&mut t.platforms);
    taste_normalize(&mut t.franchises);
    taste_normalize(&mut t.genres);
    t
}

/// How well one candidate matches the profile. Higher is better; 0 means it
/// shares nothing with what the user has played.
fn featured_affinity(game: &Game, profile: &Taste) -> f64 {
    let franchise = profile
        .franchises
        .get(&taste_key(&game.franchise))
        .copied()
        .unwrap_or(0.0);
    let platform = profile
        .platforms
        .get(&taste_key(&game.platform))
        .copied()
        .unwrap_or(0.0);
    // Genres are multi-valued: take the best-matching tag rather than the sum,
    // so a game tagged with ten genres is not mechanically favoured.
    let mut genre = 0.0_f64;
    for tag in split_genres(&game.genres) {
        genre = genre.max(profile.genres.get(&taste_key(&tag)).copied().unwrap_or(0.0));
    }
    franchise * FEATURED_FRANCHISE_WEIGHT
        + genre * FEATURED_GENRE_WEIGHT
        + platform * FEATURED_PLATFORM_WEIGHT
}

/// Normalized 0..1 critic score; games without a rating contribute nothing.
fn featured_rating(game: &Game) -> f64 {
    if game.igdb_rating > 0.0 {
        game.igdb_rating.min(100.0) / 100.0
    } else {
        0.0
    }
}

/// Ranked picks: games the user has *not* played, ordered by how well they match
/// their tracked playtime, then critic score, then title so the order is stable
/// across requests.
///
/// With no playtime recorded there is nothing to personalize against, so this
/// degrades to "the highest-rated games in the catalog". The handler only takes
/// that path when the catalog has no PC titles to draw a cold-start hero from —
/// see `is_cold_start_platform`.
fn rank_featured(games: Vec<Game>, played: &HashMap<String, i64>, limit: usize) -> Vec<Game> {
    let profile = build_taste(&games, played);
    let mut scored: Vec<(f64, Game)> = games
        .into_iter()
        .filter(|g| played.get(&g.id).copied().unwrap_or(0) <= 0)
        .map(|g| {
            let score = featured_affinity(&g, &profile) + featured_rating(&g) * FEATURED_RATING_WEIGHT;
            (score, g)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.1.igdb_rating
                    .partial_cmp(&a.1.igdb_rating)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.1.title.to_lowercase().cmp(&b.1.title.to_lowercase()))
    });
    scored.truncate(limit);
    scored.into_iter().map(|(_, g)| g).collect()
}

/// Is this one of the PC titles the cold-start hero draws from?
fn is_cold_start_platform(game: &Game) -> bool {
    game.platform
        .trim()
        .eq_ignore_ascii_case(FEATURED_COLD_START_PLATFORM)
}

/// One hero-sized pick. Carries `summary` and `heroArtUrl`, which the card
/// endpoint deliberately omits to keep the 2000-row catalog response small.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FeaturedPick {
    id: String,
    title: String,
    platform: String,
    cover_art_url: String,
    /// Wide 1080p key art; empty when IGDB had none, so the page falls back to
    /// the (portrait) cover exactly as the desktop client does.
    hero_art_url: String,
    summary: String,
    genres: Vec<String>,
    igdb_rating: f64,
    release_date: i64,
    developer: String,
    publisher: String,
    owned: bool,
}

/// The caller's game id → playtime seconds, from their own `game_stats` rows.
async fn played_seconds_for(db: &Pool, user_id: u64) -> Result<HashMap<String, i64>> {
    let mut c = db.get_conn().await?;
    let rows: Vec<(String, i64)> = c
        .exec(
            "SELECT game_id, playtime_seconds FROM game_stats WHERE user_id=:u AND playtime_seconds>0",
            params! {"u" => user_id},
        )
        .await?;
    Ok(rows.into_iter().collect())
}

// GET /api/store/featured — personalized hero picks for the storefront.
async fn store_featured(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let user = match web_user(&st, &headers).await {
        Some(u) => u,
        None => return store_signin_required(),
    };
    let games = match list_games(&st.db).await {
        Ok(g) => g,
        Err(e) => return server_error(e),
    };
    // A stats read failure should not blank the hero — fall back to the
    // unpersonalized (highest-rated) shortlist rather than erroring the page.
    let played = played_seconds_for(&st.db, user.id).await.unwrap_or_default();
    let personalized = played.values().any(|s| *s > 0);

    // Cold start: with no playtime there is nothing to rank against, and the
    // rating-ordered fallback shows the same handful of retro classics to every
    // new visitor on every load. Draw a random shortlist from the PC titles
    // instead, so the hero looks alive and surfaces the modern end of the
    // catalog rather than the same NES/SNES entries every time.
    let mut picks = if personalized {
        rank_featured(games, &played, FEATURED_LIMIT)
    } else {
        let (mut pc, rest): (Vec<Game>, Vec<Game>) =
            games.into_iter().partition(is_cold_start_platform);
        if pc.is_empty() {
            // No PC games at all — `rest` is the whole catalog here, so this is
            // the previous highest-rated behaviour, unchanged.
            rank_featured(rest, &played, FEATURED_LIMIT)
        } else {
            pc.shuffle(&mut rand::thread_rng());
            pc.truncate(FEATURED_LIMIT);
            pc
        }
    };
    let base = public_base_url(&st, &headers).await;
    for game in &mut picks {
        hydrate_server_art_url(&st, &base, game).await;
    }
    let owned = owned_set_for_request(&st, &headers).await;
    let picks: Vec<FeaturedPick> = picks
        .into_iter()
        .map(|g| FeaturedPick {
            owned: owned.contains(&g.id),
            genres: split_genres(&g.genres),
            id: g.id,
            title: g.title,
            platform: g.platform,
            cover_art_url: g.cover_art_url,
            hero_art_url: g.hero_art_url,
            summary: g.summary,
            igdb_rating: g.igdb_rating,
            release_date: g.release_date,
            developer: g.developer,
            publisher: g.publisher,
        })
        .collect();

    Json(serde_json::json!({
        "schemaVersion": 1,
        "personalized": personalized,
        "picks": picks,
    }))
    .into_response()
}
