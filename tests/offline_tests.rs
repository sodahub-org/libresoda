//! 离线单元测试：逐条移植上游 `soda/soda_test.go`、`soda/search_test.go`
//! 以及 `soda_vip_test.go` 中不依赖网络的断言。

use libresoda::model::SOURCE_SODA;
use libresoda::soda::album::extract_json_block;
use libresoda::soda::link::{extract_album_id, extract_playlist_id, extract_track_id};
use libresoda::soda::lyric::parse_soda_lyric;
use libresoda::soda::playlist::build_playlist_from_user_item;
use libresoda::soda::quality::{best_player_info, is_preview, quality_rank};
use libresoda::soda::search::android_search_url;
use libresoda::soda::track::{apply_download_info, best_from_video_model, build_song_from_track};
use libresoda::soda::types::{
    build_image_url, Album, Artist, BitRate, DownloadInfo, Image, LabelInfo, PlayerInfo,
    QualityBenefit, QualityPolicy, Track, TrackAudioInfo, TrackPlayInfo, UserPlaylistItem,
};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// 链接解析（上游 TestSodaExtractTrackIDFromText / TestSodaExtractPlaylistIDFromText）
// ---------------------------------------------------------------------------

#[test]
fn extract_track_id_from_text() {
    const ID: &str = "7304719759323564095";
    let cases = [
        ID.to_string(),
        format!("https://www.qishui.com/track/{ID}"),
        format!("https://music.douyin.com/qishui/share/track?track_id={ID}&auto_play_bgm=1"),
        format!("https://www.douyin.com/qishui/song/{ID}"),
        format!("_ROUTER_DATA = {{\"loaderData\":{{\"track_page\":{{\"track_id\":\"{ID}\"}}}}}}"),
        format!("https%3A%2F%2Fmusic.douyin.com%2Fqishui%2Fshare%2Ftrack%3Ftrack_id%3D{ID}"),
    ];

    for case in cases {
        assert_eq!(extract_track_id(&case), ID, "case: {case}");
    }
}

#[test]
fn extract_playlist_id_from_text() {
    const ID: &str = "7291667294287183907";
    let cases = [
        ID.to_string(),
        format!("https://www.qishui.com/playlist/{ID}"),
        format!("https://music.douyin.com/qishui/share/playlist?playlist_id={ID}&auto_play_bgm=1"),
        format!(
            "_ROUTER_DATA = {{\"loaderData\":{{\"playlist_page\":{{\"playlist_id\":\"{ID}\"}}}}}}"
        ),
        format!("https%3A%2F%2Fmusic.douyin.com%2Fqishui%2Fshare%2Fplaylist%3Fplaylist_id%3D{ID}"),
    ];

    for case in cases {
        assert_eq!(extract_playlist_id(&case), ID, "case: {case}");
    }
}

#[test]
fn extract_album_id_from_link() {
    assert_eq!(
        extract_album_id("https://www.qishui.com/share/album?album_id=7235800123456789"),
        "7235800123456789"
    );
    assert_eq!(
        extract_album_id("https://www.qishui.com/album/123456"),
        "123456"
    );
    assert_eq!(extract_album_id("7235800123456789"), "7235800123456789");
    assert_eq!(extract_album_id(""), "");
}

// ---------------------------------------------------------------------------
// 搜索 URL 与图片 URL（上游 search_test.go）
// ---------------------------------------------------------------------------

#[test]
fn android_search_url_uses_android_params() {
    let raw = android_search_url("track", "周杰伦", 2, 20);
    let (path, query) = raw.split_once('?').expect("query string");
    assert_eq!(path, "https://api.qishui.com/luna/search/track");

    let params = parse_query(query);
    assert_eq!(params.get("q").map(String::as_str), Some("周杰伦"));
    assert_eq!(params.get("cursor").map(String::as_str), Some("20"));
    assert_eq!(params.get("count").map(String::as_str), Some("20"));
    assert_eq!(params.get("aid").map(String::as_str), Some("386088"));
    assert_eq!(
        params.get("device_platform").map(String::as_str),
        Some("android")
    );
}

#[test]
fn build_image_url_uses_template_prefix() {
    let cover = build_image_url(
        &Image {
            urls: vec!["https://p3-luna.douyinpic.com/img/".to_string()],
            uri: "tos-cn-v-2774c002/example".to_string(),
            template_prefix: "tplv-b829550vbb".to_string(),
        },
        "~c5_300x300.jpg",
    );
    assert_eq!(
        cover,
        "https://p3-luna.douyinpic.com/img/tos-cn-v-2774c002/example~tplv-b829550vbb-resize:960:960.png"
    );
}

#[test]
fn build_image_url_appends_uri_and_suffix() {
    let cover = build_image_url(
        &Image {
            urls: vec!["https://p3-luna.douyinpic.com/img/".to_string()],
            uri: "tos-cn/cover".to_string(),
            template_prefix: String::new(),
        },
        "~c5_300x300.jpg",
    );
    assert_eq!(
        cover,
        "https://p3-luna.douyinpic.com/img/tos-cn/cover~c5_300x300.jpg"
    );
}

// ---------------------------------------------------------------------------
// 歌单构造（上游 TestSodaBuildPlaylistFromUserItem）
// ---------------------------------------------------------------------------

#[test]
fn build_playlist_from_user_item_keeps_identity_and_extra() {
    let mut item = UserPlaylistItem {
        id: "7444529378593275956".to_string(),
        title: "My Favorite Music".to_string(),
        public_title: "Tester Favorite Music".to_string(),
        playlist_type: 1,
        url_cover: Image {
            urls: vec!["https://p3-luna.douyinpic.com/img/".to_string()],
            uri: "tos-cn-v-2774c002/cover".to_string(),
            template_prefix: String::new(),
        },
        ..Default::default()
    };
    item.resource_cnt.track_cnt = 78;
    item.owner.id = "109953989288".to_string();
    item.owner.nickname = "Tester".to_string();

    let playlist = build_playlist_from_user_item(&item, "109953989288", "Tester");
    assert_eq!(playlist.id, item.id);
    assert_eq!(playlist.source, SOURCE_SODA);
    assert_eq!(playlist.track_count, 78);
    assert_eq!(playlist.creator, "Tester");
    assert_eq!(
        playlist.extra.get("user_id").map(String::as_str),
        Some("109953989288")
    );
    assert_eq!(playlist.extra.get("type").map(String::as_str), Some("1"));
    assert!(!playlist.cover.is_empty());
    assert!(!playlist.link.is_empty());
}

// ---------------------------------------------------------------------------
// VIP 标记（上游 TestSodaLabelInfoIsVIP / TestSodaBuildSongFromTrackMarksVIP）
// ---------------------------------------------------------------------------

#[test]
fn label_info_is_vip() {
    assert!(!LabelInfo::default().is_vip());

    let mut quality_map = BTreeMap::new();
    quality_map.insert(
        "lossless".to_string(),
        QualityPolicy {
            play_detail: Some(QualityBenefit {
                need_vip: true,
                ..Default::default()
            }),
            download_detail: None,
        },
    );
    let label = LabelInfo {
        quality_map,
        ..Default::default()
    };
    assert!(label.is_vip());

    let label = LabelInfo {
        only_vip_download: true,
        ..Default::default()
    };
    assert!(label.is_vip());
}

#[test]
fn build_song_from_track_marks_vip() {
    let track = Track {
        id: "7304719759323564095".to_string(),
        name: "落了白".to_string(),
        duration: 180_822,
        artists: vec![Artist {
            name: "蒋雪儿Snow.J".to_string(),
            ..Default::default()
        }],
        album: Album {
            id: "1".to_string(),
            name: "落了白".to_string(),
            ..Default::default()
        },
        bit_rates: vec![BitRate {
            size: 5_882_690,
            quality: "highest".to_string(),
            ..Default::default()
        }],
        label_info: LabelInfo {
            only_vip_download: true,
            ..Default::default()
        },
        ..Default::default()
    };

    let song = build_song_from_track(&track);
    assert!(song.is_vip);
    assert_eq!(song.extra.get("is_vip").map(String::as_str), Some("true"));
    assert_eq!(
        song.extra.get("only_vip_download").map(String::as_str),
        Some("true")
    );
    assert_eq!(song.duration, 180);
}

// ---------------------------------------------------------------------------
// 音质择优（上游 TestSodaBestPlayerInfo* / TestSodaDownloadInfoIsPreview）
// ---------------------------------------------------------------------------

#[test]
fn download_info_is_preview() {
    let short = DownloadInfo {
        duration: 60.0,
        ..Default::default()
    };
    let full = DownloadInfo {
        duration: 178.0,
        ..Default::default()
    };
    assert!(is_preview(&short, 180));
    assert!(!is_preview(&full, 180));
}

fn player_info(
    url: &str,
    duration: f64,
    quality: &str,
    format: &str,
    bitrate: i64,
    size: i64,
) -> PlayerInfo {
    PlayerInfo {
        main_play_url: url.to_string(),
        duration,
        quality: quality.to_string(),
        format: format.to_string(),
        bitrate,
        size,
        ..Default::default()
    }
}

#[test]
fn best_player_info_prefers_full_duration_then_highest_quality() {
    let list = vec![
        player_info("preview-lossless", 60.0, "lossless", "flac", 1_000_000, 12),
        player_info("full-higher", 180.0, "higher", "m4a", 320_000, 20),
        player_info("full-lossless", 180.0, "lossless", "flac", 960_000, 30),
    ];
    let best = best_player_info(&list).expect("best player info");
    assert_eq!(best.main_play_url, "full-lossless");
}

#[test]
fn best_player_info_prefers_lossless_over_spatial() {
    let list = vec![
        player_info("spatial", 180.0, "spatial", "m4a", 324_000, 8_000_000),
        player_info("lossless", 180.0, "lossless", "flac", 1_650_000, 40_000_000),
    ];
    let best = best_player_info(&list).expect("best player info");
    assert_eq!(best.main_play_url, "lossless");
}

#[test]
fn best_player_info_prefers_real_hires_over_lossless() {
    let list = vec![
        player_info("lossless", 180.0, "lossless", "flac", 960_000, 21_000_000),
        player_info("hires", 180.0, "hi_res", "flac", 2_400_000, 58_000_000),
    ];
    let best = best_player_info(&list).expect("best player info");
    assert_eq!(best.main_play_url, "hires");
}

#[test]
fn quality_rank_orders_known_tiers() {
    assert!(quality_rank("hi_res", "flac", 2_400_000) > quality_rank("lossless", "flac", 960_000));
    assert!(quality_rank("lossless", "flac", 960_000) > quality_rank("spatial", "m4a", 324_000));
    assert!(quality_rank("spatial", "m4a", 324_000) > quality_rank("higher", "m4a", 320_000));
    assert!(quality_rank("medium", "m4a", 128_000) < quality_rank("higher", "m4a", 320_000));
}

// ---------------------------------------------------------------------------
// 歌曲构造 / 下载信息应用（上游 TestSodaBuildSongFromTrackUsesHighestAudioInfoQuality）
// ---------------------------------------------------------------------------

#[test]
fn build_song_from_track_uses_highest_audio_info_quality() {
    let track = Track {
        id: "1".to_string(),
        name: "test".to_string(),
        duration: 180_000,
        audio_info: TrackAudioInfo {
            play_info_list: vec![
                TrackPlayInfo {
                    main_play_url: "higher".to_string(),
                    quality: "higher".to_string(),
                    format: "m4a".to_string(),
                    bitrate: 320_000,
                    size: 9,
                    ..Default::default()
                },
                TrackPlayInfo {
                    main_play_url: "lossless".to_string(),
                    quality: "lossless".to_string(),
                    format: "flac".to_string(),
                    bitrate: 960_000,
                    size: 8,
                    ..Default::default()
                },
            ],
        },
        ..Default::default()
    };

    let song = build_song_from_track(&track);
    assert_eq!(song.url, "lossless");
    assert_eq!(
        song.extra.get("quality").map(String::as_str),
        Some("lossless")
    );
    assert_eq!(song.ext, "flac");
    assert_eq!(song.bitrate, 960);
}

#[test]
fn apply_download_info_updates_song_fields() {
    let mut song = libresoda::model::Song::default();
    let info = DownloadInfo {
        url: "https://example.com/a.m4a".to_string(),
        play_auth: "auth token".to_string(),
        format: "m4a".to_string(),
        size: 4_000_000,
        duration: 210.2,
        bitrate: 1_200_000,
        quality: "lossless".to_string(),
        is_preview: false,
        note: String::new(),
    };
    apply_download_info(&mut song, &info);
    assert_eq!(song.url, "https://example.com/a.m4a#auth=auth+token");
    assert_eq!(song.size, 4_000_000);
    assert_eq!(song.ext, "m4a");
    assert_eq!(song.bitrate, 1200);
    assert_eq!(song.duration, 210);
    assert_eq!(
        song.extra.get("quality").map(String::as_str),
        Some("lossless")
    );
    assert_eq!(
        song.extra.get("download_quality").map(String::as_str),
        Some("lossless")
    );
}

// ---------------------------------------------------------------------------
// video_model 解析（上游 TestSodaBestFromVideoModelUsesLosslessAndSpadeAuth）
// ---------------------------------------------------------------------------

#[test]
fn best_from_video_model_uses_lossless_and_spade_auth() {
    let value: serde_json::Value = serde_json::from_str(
        r#"{
            "encrypt_info": {"spade_a": "auth-token"},
            "track": {
                "play_info_list": [
                    {"main_play_url": "standard-url", "quality": "standard", "format": "m4a", "bitrate": 128000, "size": 4000000, "duration": 210},
                    {"main_play_url": "lossless-url", "quality": "lossless", "format": "m4a", "bitrate": 1200000, "size": 32000000, "duration": 210}
                ]
            }
        }"#,
    )
    .expect("json");

    let info = best_from_video_model(&value).expect("video model stream");
    assert_eq!(info.url, "lossless-url");
    assert_eq!(info.play_auth, "auth-token");
    assert_eq!(info.quality, "lossless");
}

// ---------------------------------------------------------------------------
// 歌词与分享页 JSON 抽取
// ---------------------------------------------------------------------------

#[test]
fn parse_soda_lyric_converts_to_lrc() {
    let raw = "[0,1000]<0,100>Hello<100,900> world\n[61234,2000]第二行";
    let lrc = parse_soda_lyric(raw);
    assert_eq!(lrc, "[00:00.00]Hello world\n[01:01.23]第二行\n");
}

#[test]
fn extract_json_block_handles_nested_braces_and_strings() {
    let page =
        r#"<html>_ROUTER_DATA = {"loaderData":{"album_page":{"albumInfo":{"id":"1"}}}};</html>"#;
    let json = extract_json_block(page, "_ROUTER_DATA = ").expect("json block");
    let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    assert_eq!(value["loaderData"]["album_page"]["albumInfo"]["id"], "1");

    let page_with_brace_in_string = r#"_ROUTER_DATA = {"a":"}"}"#;
    let json =
        extract_json_block(page_with_brace_in_string, "_ROUTER_DATA = ").expect("json block");
    assert_eq!(json, r#"{"a":"}"}"#);
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

fn parse_query(query: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(
            key.to_string(),
            libresoda::util::query_unescape(value).unwrap_or_else(|| value.to_string()),
        );
    }
    map
}
