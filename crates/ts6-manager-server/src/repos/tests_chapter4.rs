//! End-to-end repo tests for the slice-2 entities (PURA-12).
//!
//! Mirrors the spec §4.5 verification list at the repo layer:
//!   - schema-roundtrip — insert one row of every entity, read it back.
//!   - cascade — server / flow / execution / playlist / song deletes
//!     propagate per §4.2 events.
//!   - no-cascade — deleting a MusicBot leaves Playlists alive with
//!     `musicBotId = null` (§4.2.12 + §4.5).
//!   - composite-unique — second insert with the same
//!     `(playlistId, songId)` MUST fail.
//!   - widget token — uniqueness enforced.
//!   - music_request — `(serverConfigId, url)` uniqueness + dedup.
//!   - app_setting seed — migration seeds `max_music_bots = "5"`.

#![allow(non_snake_case)]

use chrono::{Duration, Utc};
use surrealdb::types::SurrealValue;

use super::{
    app_settings, bot_execution_logs, bot_executions, bot_flow_runs, bot_flows, bot_variables,
    music_bots, music_requests, playlist_songs, playlists, radio_stations, server_connections,
    songs, ssh_audit_log, stream_sessions, users, widgets,
};
use crate::db::{connect_in_memory, migrations};

async fn setup() -> std::sync::Arc<crate::db::Database> {
    let db = connect_in_memory().await.expect("in-memory connect");
    migrations::run(&db).await.expect("migrations run");
    db
}

async fn seed_server(db: &crate::db::Database) -> i64 {
    server_connections::insert(
        db,
        server_connections::NewServerConnection {
            name: "primary".into(),
            host: "ts.example.com".into(),
            webqueryPort: 10080,
            apiKey: "enc:0:0:0".into(),
            useHttps: false,
            sshPort: 10022,
            sshUsername: None,
            sshPassword: None,
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            controlPath: None,
            sshAuthMethod: None,
            sshPrivateKey: None,
            sshKeyAgentSocket: None,
            sshHostKeyFingerprint: None,
        },
    )
    .await
    .expect("seed server")
    .id
}

#[tokio::test]
async fn app_setting_seed_max_music_bots_present_after_migrate() {
    // §4.2.9 contract: the migration seeds `max_music_bots = "5"` so an
    // empty database still answers the operator-facing question.
    let db = setup().await;
    let row = app_settings::get(&db, "max_music_bots")
        .await
        .expect("get max_music_bots")
        .expect("seeded row should be present");
    assert_eq!(row.key, "max_music_bots");
    assert_eq!(row.value, "5");
}

#[tokio::test]
async fn app_setting_put_is_upsert() {
    let db = setup().await;

    let inserted = app_settings::put(&db, "yt_cookie_path", "/data/yt-cookies.txt")
        .await
        .expect("first put");
    assert_eq!(inserted.value, "/data/yt-cookies.txt");

    let updated = app_settings::put(&db, "yt_cookie_path", "/data/new-cookies.txt")
        .await
        .expect("second put");
    assert_eq!(updated.value, "/data/new-cookies.txt");

    // Single row per key — re-fetch and confirm there's no duplicate.
    let all = app_settings::list(&db).await.expect("list");
    let count = all.iter().filter(|r| r.key == "yt_cookie_path").count();
    assert_eq!(count, 1, "put must upsert, not duplicate");
}

#[tokio::test]
async fn bot_flow_roundtrip_and_default_flow_data() {
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let inserted = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "greet".into(),
            description: Some("Greet new clients".into()),
            // Use the explicit empty FlowDefinition shape from §12.1 so the
            // round-trip exercises the wire format the FLOW engine reads.
            flowData: r#"{"nodes":[],"edges":[]}"#.into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: false,
        },
    )
    .await
    .expect("insert bot_flow");

    let fetched = bot_flows::find_by_id(&db, inserted.id)
        .await
        .expect("find_by_id")
        .expect("row should exist");
    assert_eq!(fetched.name, "greet");
    assert_eq!(fetched.description.as_deref(), Some("Greet new clients"));
    assert_eq!(fetched.flowData, r#"{"nodes":[],"edges":[]}"#);
    assert_eq!(fetched.serverConfigId, server_id);
    assert_eq!(fetched.virtualServerId, 1);
    assert!(!fetched.enabled);
}

#[tokio::test]
async fn bot_variable_composite_unique_flow_name_scope() {
    let db = setup().await;
    let server_id = seed_server(&db).await;
    let flow = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "f".into(),
            description: None,
            flowData: r#"{"nodes":[],"edges":[]}"#.into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: true,
        },
    )
    .await
    .unwrap();

    bot_variables::insert(
        &db,
        bot_variables::NewBotVariable {
            flowId: flow.id,
            name: "counter".into(),
            value: "0".into(),
            scope: "flow".into(),
        },
    )
    .await
    .expect("first insert");

    // Same scope — must fail.
    let dup = bot_variables::insert(
        &db,
        bot_variables::NewBotVariable {
            flowId: flow.id,
            name: "counter".into(),
            value: "1".into(),
            scope: "flow".into(),
        },
    )
    .await;
    assert!(dup.is_err(), "same (flowId, name, scope) must be rejected");

    // Different scope on the same name — allowed.
    bot_variables::insert(
        &db,
        bot_variables::NewBotVariable {
            flowId: flow.id,
            name: "counter".into(),
            value: "tmp".into(),
            scope: "temp".into(),
        },
    )
    .await
    .expect("temp-scoped sibling allowed");
}

#[tokio::test]
async fn bot_variable_delete_temp_for_flow_keeps_flow_scope() {
    let db = setup().await;
    let server_id = seed_server(&db).await;
    let flow = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "f".into(),
            description: None,
            flowData: r#"{"nodes":[],"edges":[]}"#.into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: true,
        },
    )
    .await
    .unwrap();

    bot_variables::insert(
        &db,
        bot_variables::NewBotVariable {
            flowId: flow.id,
            name: "persistent".into(),
            value: "v".into(),
            scope: "flow".into(),
        },
    )
    .await
    .unwrap();
    bot_variables::insert(
        &db,
        bot_variables::NewBotVariable {
            flowId: flow.id,
            name: "ephemeral".into(),
            value: "v".into(),
            scope: "temp".into(),
        },
    )
    .await
    .unwrap();

    bot_variables::delete_temp_for_flow(&db, flow.id)
        .await
        .expect("sweep temp");

    let remaining = bot_variables::list_for_flow(&db, flow.id).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].scope, "flow");
    assert_eq!(remaining[0].name, "persistent");
}

#[tokio::test]
async fn deleting_bot_flow_cascades_variables_executions_and_logs() {
    // §4.2 cascade: BotFlow → BotVariable, BotExecution → BotExecutionLog.
    // The chained event (bot_execution_cascade) should fire when the
    // bot_flow_cascade deletes the executions.
    let db = setup().await;
    let server_id = seed_server(&db).await;
    let flow = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "f".into(),
            description: None,
            flowData: r#"{"nodes":[],"edges":[]}"#.into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: true,
        },
    )
    .await
    .unwrap();

    bot_variables::insert(
        &db,
        bot_variables::NewBotVariable {
            flowId: flow.id,
            name: "x".into(),
            value: "1".into(),
            scope: "flow".into(),
        },
    )
    .await
    .unwrap();

    let exec = bot_executions::insert(
        &db,
        bot_executions::NewBotExecution {
            flowId: flow.id,
            triggeredBy: "manual".into(),
            triggerData: None,
        },
    )
    .await
    .unwrap();

    bot_execution_logs::insert(
        &db,
        bot_execution_logs::NewBotExecutionLog {
            executionId: Some(exec.id),
            serverConfigId: server_id,
            flowId: Some(flow.id),
            nodeId: Some("n1".into()),
            nodeName: Some("Start".into()),
            level: "info".into(),
            message: "running".into(),
            data: None,
        },
    )
    .await
    .unwrap();

    bot_flows::delete(&db, flow.id).await.expect("delete flow");

    assert!(
        bot_variables::list_for_flow(&db, flow.id)
            .await
            .unwrap()
            .is_empty(),
        "variables must cascade with the flow"
    );
    assert!(
        bot_executions::list_for_flow(&db, flow.id)
            .await
            .unwrap()
            .is_empty(),
        "executions must cascade with the flow"
    );
    assert!(
        bot_execution_logs::list_for_execution(&db, exec.id)
            .await
            .unwrap()
            .is_empty(),
        "logs must cascade through bot_execution_cascade"
    );
}

#[tokio::test]
async fn bot_execution_finish_records_status_and_ended_at() {
    let db = setup().await;
    let server_id = seed_server(&db).await;
    let flow = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "f".into(),
            description: None,
            flowData: r#"{"nodes":[],"edges":[]}"#.into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: true,
        },
    )
    .await
    .unwrap();

    let exec = bot_executions::insert(
        &db,
        bot_executions::NewBotExecution {
            flowId: flow.id,
            triggeredBy: "event".into(),
            triggerData: Some(r#"{"foo":1}"#.into()),
        },
    )
    .await
    .unwrap();
    assert_eq!(exec.status, "running");
    assert!(exec.endedAt.is_none());

    let after = bot_executions::finish(&db, exec.id, "completed", None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.status, "completed");
    assert!(after.endedAt.is_some());
}

#[tokio::test]
async fn music_bot_delete_sets_playlist_music_bot_id_null() {
    // §4.2.12 + §4.5 no-cascade test: deleting a MusicBot must NOT delete
    // its Playlists; instead `musicBotId` is nulled.
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let bot = music_bots::insert(
        &db,
        music_bots::NewMusicBot {
            name: "MB1".into(),
            serverConfigId: server_id,
            nickname: "MusicBot".into(),
            serverPassword: None,
            defaultChannel: None,
            channelPassword: None,
            nowPlayingChannelId: None,
            voicePort: 9987,
            volume: 50,
            identityData: None,
            autoStart: false,
            streamPreset: "720p".into(),
            sidecarPort: 9800,
        },
    )
    .await
    .unwrap();

    let pl = playlists::insert(
        &db,
        playlists::NewPlaylist {
            name: "Top hits".into(),
            musicBotId: Some(bot.id),
        },
    )
    .await
    .unwrap();
    assert_eq!(pl.musicBotId, Some(bot.id));

    music_bots::delete(&db, bot.id).await.expect("delete bot");

    let after = playlists::find_by_id(&db, pl.id)
        .await
        .unwrap()
        .expect("playlist must survive");
    assert_eq!(after.musicBotId, None, "musicBotId must be nulled");
    assert_eq!(after.name, "Top hits");
}

#[tokio::test]
async fn playlist_song_composite_unique_and_position_order() {
    // §4.5 composite-unique test for PlaylistSong + position-ordered list.
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let pl = playlists::insert(
        &db,
        playlists::NewPlaylist {
            name: "P".into(),
            musicBotId: None,
        },
    )
    .await
    .unwrap();

    let mut songs_v = Vec::new();
    for i in 0..3 {
        let s = songs::insert(
            &db,
            songs::NewSong {
                title: format!("song-{i}"),
                artist: None,
                duration: None,
                filePath: format!("/data/music/song-{i}.mp3"),
                source: "local".into(),
                sourceUrl: None,
                fileSize: None,
                serverConfigId: server_id,
            },
        )
        .await
        .unwrap();
        songs_v.push(s);
    }

    // Insert in reverse position order; list_for_playlist must return them
    // sorted by position, not insertion order.
    playlist_songs::insert(
        &db,
        playlist_songs::NewPlaylistSong {
            playlistId: pl.id,
            songId: songs_v[2].id,
            position: 30,
        },
    )
    .await
    .unwrap();
    playlist_songs::insert(
        &db,
        playlist_songs::NewPlaylistSong {
            playlistId: pl.id,
            songId: songs_v[0].id,
            position: 10,
        },
    )
    .await
    .unwrap();
    playlist_songs::insert(
        &db,
        playlist_songs::NewPlaylistSong {
            playlistId: pl.id,
            songId: songs_v[1].id,
            position: 20,
        },
    )
    .await
    .unwrap();

    // Composite unique: re-add song 0 to the same playlist must fail.
    let dup = playlist_songs::insert(
        &db,
        playlist_songs::NewPlaylistSong {
            playlistId: pl.id,
            songId: songs_v[0].id,
            position: 99,
        },
    )
    .await;
    assert!(dup.is_err(), "duplicate (playlistId, songId) must reject");

    let listed = playlist_songs::list_for_playlist(&db, pl.id).await.unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].songId, songs_v[0].id, "position 10 first");
    assert_eq!(listed[1].songId, songs_v[1].id, "position 20 second");
    assert_eq!(listed[2].songId, songs_v[2].id, "position 30 third");
}

#[tokio::test]
async fn deleting_song_cascades_to_playlist_song() {
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let pl = playlists::insert(
        &db,
        playlists::NewPlaylist {
            name: "P".into(),
            musicBotId: None,
        },
    )
    .await
    .unwrap();
    let song = songs::insert(
        &db,
        songs::NewSong {
            title: "T".into(),
            artist: None,
            duration: None,
            filePath: "/data/music/T.mp3".into(),
            source: "local".into(),
            sourceUrl: None,
            fileSize: None,
            serverConfigId: server_id,
        },
    )
    .await
    .unwrap();
    playlist_songs::insert(
        &db,
        playlist_songs::NewPlaylistSong {
            playlistId: pl.id,
            songId: song.id,
            position: 0,
        },
    )
    .await
    .unwrap();

    songs::delete(&db, song.id).await.expect("delete song");
    assert!(
        playlist_songs::list_for_playlist(&db, pl.id)
            .await
            .unwrap()
            .is_empty(),
        "playlist_song must cascade with song delete"
    );
}

#[tokio::test]
async fn deleting_playlist_cascades_to_playlist_song() {
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let pl = playlists::insert(
        &db,
        playlists::NewPlaylist {
            name: "P".into(),
            musicBotId: None,
        },
    )
    .await
    .unwrap();
    let song = songs::insert(
        &db,
        songs::NewSong {
            title: "T".into(),
            artist: None,
            duration: None,
            filePath: "/data/music/T.mp3".into(),
            source: "local".into(),
            sourceUrl: None,
            fileSize: None,
            serverConfigId: server_id,
        },
    )
    .await
    .unwrap();
    playlist_songs::insert(
        &db,
        playlist_songs::NewPlaylistSong {
            playlistId: pl.id,
            songId: song.id,
            position: 0,
        },
    )
    .await
    .unwrap();

    playlists::delete(&db, pl.id)
        .await
        .expect("delete playlist");
    assert!(
        playlist_songs::list_for_playlist(&db, pl.id)
            .await
            .unwrap()
            .is_empty(),
        "playlist_song must cascade with playlist delete"
    );
}

#[tokio::test]
async fn radio_station_roundtrip_and_list_for_server() {
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let rs = radio_stations::insert(
        &db,
        radio_stations::NewRadioStation {
            name: "BBC R1".into(),
            url: "https://stream.example/radio.mp3".into(),
            genre: Some("pop".into()),
            imageUrl: None,
            serverConfigId: server_id,
        },
    )
    .await
    .unwrap();
    assert_eq!(rs.url, "https://stream.example/radio.mp3");

    let list = radio_stations::list_for_server(&db, server_id)
        .await
        .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, rs.id);
}

#[tokio::test]
async fn widget_token_unique_and_find_by_token() {
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let w = widgets::insert(
        &db,
        widgets::NewWidget {
            name: "lobby".into(),
            token: "tok-aaaa".into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            theme: "dark".into(),
            showChannelTree: true,
            showClients: true,
            hideEmptyChannels: false,
            maxChannelDepth: 5,
        },
    )
    .await
    .unwrap();

    let dup = widgets::insert(
        &db,
        widgets::NewWidget {
            name: "lobby2".into(),
            token: "tok-aaaa".into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            theme: "dark".into(),
            showChannelTree: true,
            showClients: true,
            hideEmptyChannels: false,
            maxChannelDepth: 5,
        },
    )
    .await;
    assert!(dup.is_err(), "duplicate widget token must be rejected");

    let found = widgets::find_by_token(&db, "tok-aaaa")
        .await
        .unwrap()
        .expect("token lookup");
    assert_eq!(found.id, w.id);
    assert_eq!(found.theme, "dark");
}

#[tokio::test]
async fn music_request_dedup_via_record() {
    // §4.2.16 dedup: re-requesting the same `(serverConfigId, url)` MUST
    // NOT create a duplicate row. `record` exposes the dedup contract.
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let first = music_requests::record(
        &db,
        music_requests::NewMusicRequest {
            title: "Song A".into(),
            url: "https://yt.example/a".into(),
            serverConfigId: server_id,
        },
    )
    .await
    .unwrap();

    let second = music_requests::record(
        &db,
        music_requests::NewMusicRequest {
            title: "Song A renamed".into(),
            url: "https://yt.example/a".into(),
            serverConfigId: server_id,
        },
    )
    .await
    .unwrap();

    assert_eq!(first.id, second.id, "second record() must reuse the row");

    let listed = music_requests::list_for_server(&db, server_id)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);

    // Direct insert must still surface the index violation.
    let dup = music_requests::insert(
        &db,
        music_requests::NewMusicRequest {
            title: "again".into(),
            url: "https://yt.example/a".into(),
            serverConfigId: server_id,
        },
    )
    .await;
    assert!(dup.is_err(), "duplicate (serverConfigId, url) must reject");
}

#[tokio::test]
async fn deleting_server_cascades_chapter4_entities_except_logs() {
    // §4.2 cascade for TsServerConfig — every per-server entity disappears
    // except bot_execution_log, whose FK is intentionally non-cascading
    // per §4.2.8.
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let flow = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "f".into(),
            description: None,
            flowData: r#"{"nodes":[],"edges":[]}"#.into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: true,
        },
    )
    .await
    .unwrap();

    music_bots::insert(
        &db,
        music_bots::NewMusicBot {
            name: "MB".into(),
            serverConfigId: server_id,
            nickname: "MusicBot".into(),
            serverPassword: None,
            defaultChannel: None,
            channelPassword: None,
            nowPlayingChannelId: None,
            voicePort: 9987,
            volume: 50,
            identityData: None,
            autoStart: false,
            streamPreset: "720p".into(),
            sidecarPort: 9800,
        },
    )
    .await
    .unwrap();

    songs::insert(
        &db,
        songs::NewSong {
            title: "T".into(),
            artist: None,
            duration: None,
            filePath: "/data/music/T.mp3".into(),
            source: "local".into(),
            sourceUrl: None,
            fileSize: None,
            serverConfigId: server_id,
        },
    )
    .await
    .unwrap();

    radio_stations::insert(
        &db,
        radio_stations::NewRadioStation {
            name: "R".into(),
            url: "https://r.example/s.mp3".into(),
            genre: None,
            imageUrl: None,
            serverConfigId: server_id,
        },
    )
    .await
    .unwrap();

    widgets::insert(
        &db,
        widgets::NewWidget {
            name: "w".into(),
            token: "tok-w".into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            theme: "dark".into(),
            showChannelTree: true,
            showClients: true,
            hideEmptyChannels: false,
            maxChannelDepth: 5,
        },
    )
    .await
    .unwrap();

    music_requests::insert(
        &db,
        music_requests::NewMusicRequest {
            title: "Q".into(),
            url: "https://yt.example/q".into(),
            serverConfigId: server_id,
        },
    )
    .await
    .unwrap();

    // Engine-level log (executionId = None). After server delete this row
    // must SURVIVE — §4.2.8 says the FK doesn't cascade.
    bot_execution_logs::insert(
        &db,
        bot_execution_logs::NewBotExecutionLog {
            executionId: None,
            serverConfigId: server_id,
            flowId: Some(flow.id),
            nodeId: None,
            nodeName: None,
            level: "info".into(),
            message: "engine boot".into(),
            data: None,
        },
    )
    .await
    .unwrap();

    server_connections::delete(&db, server_id)
        .await
        .expect("delete server");

    assert!(
        bot_flows::list_for_server(&db, server_id)
            .await
            .unwrap()
            .is_empty(),
        "bot_flow must cascade"
    );
    assert!(
        music_bots::list_for_server(&db, server_id)
            .await
            .unwrap()
            .is_empty(),
        "music_bot must cascade"
    );
    assert!(
        songs::list_for_server(&db, server_id)
            .await
            .unwrap()
            .is_empty(),
        "song must cascade"
    );
    assert!(
        radio_stations::list_for_server(&db, server_id)
            .await
            .unwrap()
            .is_empty(),
        "radio_station must cascade"
    );
    assert!(
        widgets::list_for_server(&db, server_id)
            .await
            .unwrap()
            .is_empty(),
        "widget must cascade"
    );
    assert!(
        music_requests::list_for_server(&db, server_id)
            .await
            .unwrap()
            .is_empty(),
        "music_request must cascade"
    );

    let logs = bot_execution_logs::list_for_flow(&db, flow.id)
        .await
        .unwrap();
    assert_eq!(
        logs.len(),
        1,
        "bot_execution_log must NOT cascade — §4.2.8 FK is non-cascading"
    );
}

#[tokio::test]
async fn stream_session_finish_stamps_ended_at_and_peak() {
    let db = setup().await;
    let server_id = seed_server(&db).await;
    let bot = music_bots::insert(
        &db,
        music_bots::NewMusicBot {
            name: "MB".into(),
            serverConfigId: server_id,
            nickname: "MusicBot".into(),
            serverPassword: None,
            defaultChannel: None,
            channelPassword: None,
            nowPlayingChannelId: None,
            voicePort: 9987,
            volume: 50,
            identityData: None,
            autoStart: false,
            streamPreset: "720p".into(),
            sidecarPort: 9800,
        },
    )
    .await
    .unwrap();

    let session = stream_sessions::insert(
        &db,
        stream_sessions::NewStreamSession {
            musicBotId: bot.id,
            source: "https://yt.example/v".into(),
            preset: "720p".into(),
        },
    )
    .await
    .unwrap();
    assert!(session.endedAt.is_none());
    assert_eq!(session.peakViewers, 0);

    let after = stream_sessions::finish(&db, session.id, 17)
        .await
        .unwrap()
        .unwrap();
    assert!(after.endedAt.is_some());
    assert_eq!(after.peakViewers, 17);

    // §4.2.17: musicBotId is informational; deleting the bot does NOT
    // touch stream_session rows.
    music_bots::delete(&db, bot.id).await.unwrap();
    let still_there = stream_sessions::find_by_id(&db, session.id).await.unwrap();
    assert!(
        still_there.is_some(),
        "stream_session survives MusicBot delete (informational FK)"
    );
}

#[tokio::test]
async fn migrate_run_is_idempotent_on_repeat() {
    // Spec §4.5 migration-replay coverage: applying migrations twice on
    // the same database must be a no-op the second time. With a seeded
    // app_setting row in 0004 a non-idempotent re-run would CREATE a
    // duplicate and fail.
    let db = setup().await;

    let report = migrations::run(&db).await.expect("second run");
    assert!(report.applied.is_empty(), "second run must apply nothing");
    assert!(
        report
            .skipped
            .iter()
            .any(|s| s == "0004_chapter4_remaining_entities"),
        "0004 must appear in the skipped set"
    );

    // The seed must not have duplicated.
    let all = app_settings::list(&db).await.unwrap();
    let seeded = all.iter().filter(|r| r.key == "max_music_bots").count();
    assert_eq!(seeded, 1, "max_music_bots must remain a single row");
}

#[tokio::test]
async fn schema_roundtrip_one_row_per_entity() {
    // §4.5 schema-roundtrip — minimal smoke test that every chapter-4
    // remainder entity inserts and reads back without losing fields.
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let flow = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "f".into(),
            description: None,
            flowData: r#"{"nodes":[],"edges":[]}"#.into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: true,
        },
    )
    .await
    .unwrap();
    bot_variables::insert(
        &db,
        bot_variables::NewBotVariable {
            flowId: flow.id,
            name: "k".into(),
            value: "v".into(),
            scope: "flow".into(),
        },
    )
    .await
    .unwrap();
    let exec = bot_executions::insert(
        &db,
        bot_executions::NewBotExecution {
            flowId: flow.id,
            triggeredBy: "manual".into(),
            triggerData: None,
        },
    )
    .await
    .unwrap();
    bot_execution_logs::insert(
        &db,
        bot_execution_logs::NewBotExecutionLog {
            executionId: Some(exec.id),
            serverConfigId: server_id,
            flowId: Some(flow.id),
            nodeId: None,
            nodeName: None,
            level: "info".into(),
            message: "hi".into(),
            data: None,
        },
    )
    .await
    .unwrap();
    app_settings::put(&db, "k", "v").await.unwrap();
    let bot = music_bots::insert(
        &db,
        music_bots::NewMusicBot {
            name: "MB".into(),
            serverConfigId: server_id,
            nickname: "MusicBot".into(),
            serverPassword: None,
            defaultChannel: None,
            channelPassword: None,
            nowPlayingChannelId: None,
            voicePort: 9987,
            volume: 50,
            identityData: None,
            autoStart: false,
            streamPreset: "720p".into(),
            sidecarPort: 9800,
        },
    )
    .await
    .unwrap();
    let song = songs::insert(
        &db,
        songs::NewSong {
            title: "T".into(),
            artist: Some("A".into()),
            duration: Some(123.4),
            filePath: "/data/music/T.mp3".into(),
            source: "local".into(),
            sourceUrl: None,
            fileSize: Some(4096),
            serverConfigId: server_id,
        },
    )
    .await
    .unwrap();
    let pl = playlists::insert(
        &db,
        playlists::NewPlaylist {
            name: "P".into(),
            musicBotId: Some(bot.id),
        },
    )
    .await
    .unwrap();
    playlist_songs::insert(
        &db,
        playlist_songs::NewPlaylistSong {
            playlistId: pl.id,
            songId: song.id,
            position: 0,
        },
    )
    .await
    .unwrap();
    radio_stations::insert(
        &db,
        radio_stations::NewRadioStation {
            name: "R".into(),
            url: "https://r.example/s.mp3".into(),
            genre: Some("pop".into()),
            imageUrl: Some("https://r.example/i.png".into()),
            serverConfigId: server_id,
        },
    )
    .await
    .unwrap();
    widgets::insert(
        &db,
        widgets::NewWidget {
            name: "w".into(),
            token: "tok-rt".into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            theme: "dark".into(),
            showChannelTree: true,
            showClients: true,
            hideEmptyChannels: false,
            maxChannelDepth: 5,
        },
    )
    .await
    .unwrap();
    music_requests::insert(
        &db,
        music_requests::NewMusicRequest {
            title: "Q".into(),
            url: "https://yt.example/q".into(),
            serverConfigId: server_id,
        },
    )
    .await
    .unwrap();
    stream_sessions::insert(
        &db,
        stream_sessions::NewStreamSession {
            musicBotId: bot.id,
            source: "https://yt.example/v".into(),
            preset: "720p".into(),
        },
    )
    .await
    .unwrap();

    // Read every entity back via list/find and confirm identity.
    assert_eq!(bot_flows::list(&db).await.unwrap().len(), 1);
    assert_eq!(
        bot_variables::list_for_flow(&db, flow.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        bot_executions::list_for_flow(&db, flow.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        bot_execution_logs::list_for_execution(&db, exec.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(app_settings::get(&db, "k").await.unwrap().is_some());
    assert_eq!(music_bots::list(&db).await.unwrap().len(), 1);
    assert_eq!(songs::list(&db).await.unwrap().len(), 1);
    assert_eq!(playlists::list(&db).await.unwrap().len(), 1);
    assert_eq!(
        playlist_songs::list_for_playlist(&db, pl.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        radio_stations::list_for_server(&db, server_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        widgets::list_for_server(&db, server_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        music_requests::list_for_server(&db, server_id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        stream_sessions::list_for_music_bot(&db, bot.id)
            .await
            .unwrap()
            .len(),
        1
    );

    // Suppress unused-variable warnings on `_` bindings — explicitly drop.
    let _ = (flow, exec, song, pl, bot);
    // Time-fence so the `expires_at` style fields settle when running with
    // a fast clock.
    let _ = Utc::now() + Duration::seconds(1);
}

// =====================================================================
// PURA-79 — `ssh_audit_log` repo + persistence integration tests.
//
// Covers the SecurityEngineer-required R-list (sign-off on PURA-79):
//   R2 — per-field caps: errorMsg 4 KiB, commandLine 8 KiB, with sentinel.
//   R7 — userId set-null on user delete; serverConfigId NON-cascading.
//   end-to-end — AuditEntry::persist round-trips through the table.
//
// The R3 / R4 / R5 unit tests live next to the code they protect:
//   R3 (debug_assert credential belt)        → repos/ssh_audit_log.rs::tests
//   R4 (INSERT-only static-grep)             → repos/ssh_audit_log.rs::tests
//   R5 (persist_failed field grep + no-panic) → sshbridge/audit.rs::tests
// =====================================================================

async fn seed_user(db: &crate::db::Database) -> i64 {
    users::insert(
        db,
        users::NewUser {
            username: format!(
                "op-{}",
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
            ),
            passwordHash: "$argon2id$v=19$m=64,t=1,p=1$YWFhYWFhYWE$ZmFrZQ".into(),
            displayName: "Operator".into(),
            role: "admin".into(),
            enabled: true,
        },
    )
    .await
    .expect("seed user")
    .id
}

fn new_audit_log(server_id: i64, user_id: Option<i64>) -> ssh_audit_log::NewSshAuditLog {
    ssh_audit_log::NewSshAuditLog {
        serverConfigId: server_id,
        virtualServerId: Some(1),
        userId: user_id,
        command: "clientlist".into(),
        commandLine: "clientlist -uid".into(),
        exitCode: 0,
        outcome: "success".into(),
        errorMsg: String::new(),
        completedAt: chrono::Utc::now(),
        latencyMs: 15,
    }
}

#[tokio::test]
async fn ssh_audit_log_insert_roundtrips_every_field() {
    let db = setup().await;
    let server_id = seed_server(&db).await;
    let user_id = seed_user(&db).await;

    let row = ssh_audit_log::insert(&db, new_audit_log(server_id, Some(user_id)))
        .await
        .expect("insert");

    assert!(row.id > 0, "id is server-allocated and > 0");
    assert_eq!(row.serverConfigId, server_id);
    assert_eq!(row.virtualServerId, Some(1));
    assert_eq!(row.userId, Some(user_id));
    assert_eq!(row.command, "clientlist");
    assert_eq!(row.commandLine, "clientlist -uid");
    assert_eq!(row.exitCode, 0);
    assert_eq!(row.outcome, "success");
    assert_eq!(row.errorMsg, "");
    assert_eq!(row.latencyMs, 15);
    // insertedAt is server-stamped — non-zero is enough; we don't pin a
    // specific value, just confirm the field is populated.
    assert!(row.insertedAt.timestamp() > 0);
}

#[tokio::test]
async fn ssh_audit_log_insert_caps_errormsg_at_4kib_with_sentinel() {
    // PURA-79 R2: a pathological upstream message must be truncated at the
    // persistence boundary; original byte length recorded in the sentinel.
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let pathological = "x".repeat(10_000);
    let mut new = new_audit_log(server_id, None);
    new.errorMsg = pathological.clone();

    let row = ssh_audit_log::insert(&db, new).await.expect("insert");
    assert!(
        row.errorMsg.len() <= ssh_audit_log::ERROR_MSG_MAX_BYTES,
        "errorMsg must respect 4 KiB cap, got {} bytes",
        row.errorMsg.len()
    );
    assert!(
        row.errorMsg.contains("[truncated, original 10000 bytes]"),
        "truncation sentinel must record the original byte length"
    );
}

#[tokio::test]
async fn ssh_audit_log_insert_caps_command_line_at_8kib_with_sentinel() {
    let db = setup().await;
    let server_id = seed_server(&db).await;

    // Build a long but credential-clean command line so R3's debug_assert
    // doesn't fire — pad with a long flag list rather than `password=`.
    let pad = "-flag ".repeat(2_000); // 12_000 bytes
    let pathological = format!("clientlist {pad}");
    let original_len = pathological.len();

    let mut new = new_audit_log(server_id, None);
    new.commandLine = pathological;

    let row = ssh_audit_log::insert(&db, new).await.expect("insert");
    assert!(
        row.commandLine.len() <= ssh_audit_log::COMMAND_LINE_MAX_BYTES,
        "commandLine must respect 8 KiB cap, got {} bytes",
        row.commandLine.len()
    );
    assert!(
        row.commandLine
            .contains(&format!("[truncated, original {original_len} bytes]")),
        "commandLine truncation sentinel must record the original byte length"
    );
}

#[tokio::test]
async fn ssh_audit_log_user_delete_sets_user_id_null_audit_row_survives() {
    // PURA-79 R7: deleting a user nulls `userId` on the audit row but does
    // NOT delete it. Mirrors `music_bot_set_null_playlist` (§4.2.12 / §4.5).
    let db = setup().await;
    let server_id = seed_server(&db).await;
    let user_id = seed_user(&db).await;

    let row = ssh_audit_log::insert(&db, new_audit_log(server_id, Some(user_id)))
        .await
        .expect("insert");
    assert_eq!(row.userId, Some(user_id));

    users::delete(&db, user_id).await.expect("delete user");

    // Read every audit row back via a passthrough query — the repo deliberately
    // does NOT export a single-row find (R4: INSERT-only repo), so the test
    // uses a raw projection here.
    #[derive(Debug, serde::Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct AuditUserId {
        id: i64,
        userId: Option<i64>,
    }
    let mut resp = db
        .query(
            "SELECT record::id(id) AS id, userId FROM ssh_audit_log
                WHERE record::id(id) = $id;",
        )
        .bind(("id", row.id))
        .await
        .unwrap()
        .check()
        .unwrap();
    let after: Vec<AuditUserId> = resp.take(0).unwrap();
    assert_eq!(after.len(), 1, "audit row MUST survive user delete");
    assert_eq!(
        after[0].userId, None,
        "userId MUST be set to null on user delete (set-null, not cascade)"
    );
}

#[tokio::test]
async fn ssh_audit_log_server_delete_does_not_cascade_audit_row_survives() {
    // PURA-79: `serverConfigId` is intentionally NON-cascading per the
    // `bot_execution_log` precedent (§4.2.8). Audit history must outlive
    // server-config deletion — that is the entire point of an audit trail.
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let row = ssh_audit_log::insert(&db, new_audit_log(server_id, None))
        .await
        .expect("insert");

    server_connections::delete(&db, server_id)
        .await
        .expect("delete server");

    let mut resp = db
        .query(
            "SELECT record::id(id) AS id FROM ssh_audit_log
                WHERE record::id(id) = $id;",
        )
        .bind(("id", row.id))
        .await
        .unwrap()
        .check()
        .unwrap();
    #[derive(serde::Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct OnlyId {
        id: i64,
    }
    let after: Vec<OnlyId> = resp.take(0).unwrap();
    assert_eq!(
        after.len(),
        1,
        "audit row MUST survive server_connection delete"
    );
}

#[tokio::test]
async fn ssh_audit_log_prune_older_than_chunks_oversize_set() {
    // PURA-79 R6: prune_older_than chunks at 1000 rows — verify by inserting
    // > chunk-size rows back-dated, calling prune, and confirming all
    // back-dated rows are gone while a recent row survives.
    let db = setup().await;
    let server_id = seed_server(&db).await;

    let old_completed = chrono::Utc::now() - chrono::Duration::days(400);
    let recent_completed = chrono::Utc::now();

    // 1010 old rows — straddles the 1000-row chunk so the inner loop must
    // iterate twice.
    for _ in 0..1010 {
        let mut new = new_audit_log(server_id, None);
        new.completedAt = old_completed;
        ssh_audit_log::insert(&db, new).await.expect("insert old");
    }
    // One recent row that MUST survive.
    let mut keeper_new = new_audit_log(server_id, None);
    keeper_new.completedAt = recent_completed;
    let keeper = ssh_audit_log::insert(&db, keeper_new)
        .await
        .expect("insert recent");

    let cutoff = chrono::Utc::now() - chrono::Duration::days(365);
    let pruned = ssh_audit_log::prune_older_than(&db, cutoff)
        .await
        .expect("prune");
    assert_eq!(
        pruned, 1010,
        "prune count must include every row across both chunks"
    );

    // Recent row survives.
    let mut resp = db
        .query("SELECT record::id(id) AS id FROM ssh_audit_log;")
        .await
        .unwrap()
        .check()
        .unwrap();
    #[derive(serde::Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct Surv {
        id: i64,
    }
    let survivors: Vec<Surv> = resp.take(0).unwrap();
    assert_eq!(survivors.len(), 1, "only the recent row must remain");
    assert_eq!(survivors[0].id, keeper.id);
}

#[tokio::test]
async fn audit_entry_persist_round_trips_through_table() {
    // End-to-end: AuditEntry::emit + persist writes a row that the repo
    // can read back. Confirms the shaping inside `persist_inner` matches
    // the table definition.
    use crate::sshbridge::audit::{AuditEntry, AuditOutcome};
    use std::time::Duration as StdDuration;

    let db = setup().await;
    let server_id = seed_server(&db).await;
    let user_id = seed_user(&db).await;

    let entry = AuditEntry::success(
        server_id,
        Some(1),
        Some(user_id),
        "clientlist -uid",
        StdDuration::from_millis(42),
    );
    entry.emit();
    entry.persist(&db).await;

    let mut resp = db
        .query(
            "SELECT record::id(id) AS id, command, commandLine, outcome, exitCode, latencyMs, userId
                FROM ssh_audit_log;",
        )
        .await
        .unwrap()
        .check()
        .unwrap();

    #[allow(non_snake_case)]
    #[derive(serde::Deserialize, SurrealValue)]
    #[surreal(crate = "surrealdb::types")]
    struct AuditMin {
        id: i64,
        command: String,
        commandLine: String,
        outcome: String,
        exitCode: i64,
        latencyMs: i64,
        userId: Option<i64>,
    }
    let rows: Vec<AuditMin> = resp.take(0).unwrap();
    assert_eq!(rows.len(), 1, "persist must produce exactly one row");
    let r = &rows[0];
    assert_eq!(r.command, "clientlist");
    assert_eq!(r.commandLine, "clientlist -uid");
    assert_eq!(r.outcome, AuditOutcome::Success.as_db_string());
    assert_eq!(r.exitCode, 0);
    assert_eq!(r.latencyMs, 42);
    assert_eq!(r.userId, Some(user_id));
}

// =====================================================================
// PURA-265 — flow-engine v2 wire types + persistence.
//
// The v2 graph rides the *existing* opaque `bot_flow.flowData` column as a
// versioned envelope, and v2 per-node run records ride the existing opaque
// `bot_flow_run.actionResults` column — zero SurrealDB schema migration.
// These tests prove both round-trip at the repo boundary and that a legacy
// v1.1 row still decodes, via the projection shim.
// =====================================================================

/// Hand-built multi-node graph: `trigger → branch → {welcome, fallback}`.
/// Exercises 4 node kinds and a fan-out so the round-trip is non-trivial.
fn sample_v2_graph() -> ts6_manager_shared::flows::v2::FlowGraph {
    use ts6_manager_shared::flows::v2::*;
    use ts6_manager_shared::flows::{Action, Trigger};

    FlowGraph {
        nodes: vec![
            Node {
                id: NodeId("start".into()),
                label: None,
                position: Position { x: 0.0, y: 0.0 },
                kind: NodeKind::Trigger {
                    config: Trigger::Ts6ClientJoined {
                        channel_id: Some(5),
                    },
                },
            },
            Node {
                id: NodeId("by_channel".into()),
                label: Some("Route by channel".into()),
                position: Position { x: 0.0, y: 120.0 },
                kind: NodeKind::Branch {
                    cases: vec![BranchCase {
                        label: "lobby".into(),
                        when: "trigger.channelId == 1".into(),
                    }],
                },
            },
            Node {
                id: NodeId("welcome".into()),
                label: None,
                position: Position { x: -80.0, y: 240.0 },
                kind: NodeKind::Action {
                    config: Action::LogLine {
                        message: "welcome".into(),
                    },
                },
            },
            Node {
                id: NodeId("fallback".into()),
                label: None,
                position: Position { x: 80.0, y: 240.0 },
                kind: NodeKind::Action {
                    config: Action::LogLine {
                        message: "fallback".into(),
                    },
                },
            },
        ],
        edges: vec![
            Edge {
                id: EdgeId("e0".into()),
                from: PortRef {
                    node: NodeId("start".into()),
                    port: "out".into(),
                },
                to: PortRef {
                    node: NodeId("by_channel".into()),
                    port: "in".into(),
                },
                join_policy: JoinPolicy::All,
            },
            Edge {
                id: EdgeId("e1".into()),
                from: PortRef {
                    node: NodeId("by_channel".into()),
                    port: "lobby".into(),
                },
                to: PortRef {
                    node: NodeId("welcome".into()),
                    port: "in".into(),
                },
                join_policy: JoinPolicy::All,
            },
            Edge {
                id: EdgeId("e2".into()),
                from: PortRef {
                    node: NodeId("by_channel".into()),
                    port: "default".into(),
                },
                to: PortRef {
                    node: NodeId("fallback".into()),
                    port: "in".into(),
                },
                join_policy: JoinPolicy::All,
            },
        ],
    }
}

#[tokio::test]
async fn bot_flow_v2_graph_round_trips_through_flow_data_column() {
    use ts6_manager_shared::flows::v2;

    let db = setup().await;
    let server_id = seed_server(&db).await;

    let graph = sample_v2_graph();
    // The v2 envelope `{ "version": 2, "graph": { … } }` is just a JSON
    // string in the *existing* `flowData` column — no schema migration.
    let flow_data = v2::encode_flow_data(&graph);
    assert!(
        flow_data.contains(r#""version":2"#),
        "envelope: {flow_data}"
    );

    let inserted = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "graph-flow".into(),
            description: None,
            flowData: flow_data,
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: false,
        },
    )
    .await
    .expect("insert v2 bot_flow");

    let fetched = bot_flows::find_by_id(&db, inserted.id)
        .await
        .expect("find_by_id")
        .expect("row exists");

    // The repo deserializer reports v2 and decodes the multi-node graph
    // back identically.
    assert_eq!(v2::flow_version(&fetched.flowData).unwrap(), 2);
    let decoded = v2::decode_flow_data(&fetched.flowData).expect("decode v2 envelope");
    assert_eq!(decoded, graph);
}

#[tokio::test]
async fn bot_flow_legacy_row_deserializes_via_projection_shim() {
    use ts6_manager_shared::flows::v2::{self, NodeKind};

    let db = setup().await;
    let server_id = seed_server(&db).await;

    // A legacy v1.1 row: a bare `FlowDefinition` with no `version` key —
    // exactly what the v1.1 engine wrote before PURA-259.
    let legacy_flow_data = r#"{"trigger":{"kind":"ts6ClientJoined","channelId":3},"actions":[{"kind":"logLine","message":"a"},{"kind":"logLine","message":"b"}]}"#;

    let inserted = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "legacy-flow".into(),
            description: None,
            flowData: legacy_flow_data.into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: false,
        },
    )
    .await
    .expect("insert legacy bot_flow");

    let fetched = bot_flows::find_by_id(&db, inserted.id)
        .await
        .expect("find_by_id")
        .expect("row exists");

    // No `version` key ⇒ flowVersion 1; the deserializer falls back to the
    // v1.1 `FlowDefinition` and projects it into a degenerate path graph.
    assert_eq!(v2::flow_version(&fetched.flowData).unwrap(), 1);
    let graph = v2::decode_flow_data(&fetched.flowData).expect("decode legacy via shim");
    assert_eq!(graph.nodes.len(), 3, "trigger + one node per action");
    assert_eq!(graph.edges.len(), 2, "chained path graph");
    assert!(matches!(graph.nodes[0].kind, NodeKind::Trigger { .. }));
    assert!(matches!(graph.nodes[1].kind, NodeKind::Action { .. }));
    assert!(matches!(graph.nodes[2].kind, NodeKind::Action { .. }));
}

#[tokio::test]
async fn bot_flow_run_node_results_round_trip_in_action_results_column() {
    use ts6_manager_shared::flows::FlowRunStatus;
    use ts6_manager_shared::flows::v2::{NodeId, NodeResult, NodeStatus};

    let db = setup().await;
    let server_id = seed_server(&db).await;
    let flow = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "v2-run".into(),
            description: None,
            flowData: r#"{"version":2,"graph":{"nodes":[],"edges":[]}}"#.into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: true,
        },
    )
    .await
    .expect("insert bot_flow");

    let started = Utc::now();
    let node_results = vec![
        NodeResult {
            node_id: NodeId("start".into()),
            kind: "trigger".into(),
            status: NodeStatus::Ok,
            started_at: started,
            finished_at: Some(started),
            duration_ms: Some(0),
            error: None,
            output: Some(serde_json::json!({ "channelId": 5 })),
        },
        NodeResult {
            node_id: NodeId("welcome".into()),
            kind: "action".into(),
            status: NodeStatus::Errored,
            started_at: started,
            finished_at: Some(started),
            duration_ms: Some(318),
            error: Some("client not on server".into()),
            output: None,
        },
    ];

    let inserted = bot_flow_runs::insert(
        &db,
        bot_flow_runs::NewBotFlowRun {
            flowId: flow.id,
            trigger: serde_json::json!({ "kind": "manualFire" }),
            status: FlowRunStatus::InFlight,
            // A v2 run records per-node results and leaves actionResults
            // empty — both ride the one opaque `actionResults` column.
            actionResults: vec![],
            nodeResults: node_results.clone(),
        },
    )
    .await
    .expect("insert v2 bot_flow_run");

    let fetched = bot_flow_runs::find_by_id(&db, inserted.id)
        .await
        .expect("find_by_id")
        .expect("run row exists");
    assert!(
        fetched.actionResults.is_empty(),
        "v2 run leaves actionResults empty"
    );
    assert_eq!(fetched.nodeResults, node_results, "nodeResults round-trip");
}
