//! 应用级签名凭证（`x-helios` / `x-medusa`）的离线行为。
//!
//! 这些断言锁住「VIP 整曲取流」这条链路的可测部分：凭证解析、设备指纹注入、
//! 请求头注入。真实的整曲回包验证在 `tests/network_tests.rs` 的
//! `stream_access_diagnose_online`（需要抓包得到的真实凭证）。

use libresoda::soda::pc_app_params_with;
use libresoda::soda::{pc_track_v2_url, pc_track_v2_url_with};
use libresoda::AppCredentials;
use libresoda::Soda;

fn sample_credentials() -> AppCredentials {
    AppCredentials::from_json(
        r#"{
            "deviceId": "7123456789012345",
            "iid": "7123456789012346",
            "X-Helios": "helios-token-value",
            "xMedusa": "medusa-token-value"
        }"#,
    )
    .expect("camelCase / 大写头名别名都应能解析")
}

#[test]
fn credentials_parse_aliases_and_completeness() {
    let credentials = sample_credentials().normalized();
    assert_eq!(credentials.device_id, "7123456789012345");
    assert_eq!(credentials.iid, "7123456789012346");
    assert_eq!(credentials.x_helios, "helios-token-value");
    assert_eq!(credentials.x_medusa, "medusa-token-value");
    assert!(credentials.is_complete());

    // fp 缺省时回落到 device_id（官方客户端两者一致）
    assert_eq!(credentials.fp_or_device_id(), "7123456789012345");

    // 只有一半凭证不算可用，避免半吊子配置静默拿试听
    let partial = AppCredentials {
        device_id: "7123456789012345".to_string(),
        x_helios: "only-helios".to_string(),
        ..Default::default()
    };
    assert!(!partial.is_complete());
}

#[test]
fn app_signature_headers_follow_credentials() {
    let soda = Soda::new("sessionid_ss=test");
    assert!(
        soda.app_signature_headers().is_empty(),
        "未配置凭证时不应注入签名头"
    );

    soda.set_app_credentials(sample_credentials());
    let headers = soda.app_signature_headers();
    let names: Vec<&str> = headers.iter().map(|(name, _)| name.as_str()).collect();
    assert!(names.contains(&"x-helios"));
    assert!(names.contains(&"x-medusa"));
    let helios = headers
        .iter()
        .find(|(name, _)| name == "x-helios")
        .map(|(_, value)| value.as_str())
        .unwrap();
    assert_eq!(helios, "helios-token-value");

    soda.clear_app_credentials();
    assert!(soda.app_signature_headers().is_empty());
}

#[test]
fn pc_params_use_captured_device_fingerprint() {
    let credentials = sample_credentials();
    let params = pc_app_params_with(Some(&credentials));
    assert_eq!(params.get("device_id"), Some("7123456789012345"));
    assert_eq!(params.get("iid"), Some("7123456789012346"));
    assert_eq!(params.get("fp"), Some("7123456789012345"));
    assert_eq!(params.get("aid"), Some("386088"));
    assert_eq!(params.get("app_name"), Some("luna_pc"));

    let url = pc_track_v2_url_with(Some(&credentials));
    assert!(url.starts_with("https://api.qishui.com/luna/pc/track_v2?"));
    assert!(url.contains("device_id=7123456789012345"));
    assert!(url.contains("fp=7123456789012345"));
}

#[test]
fn pc_params_without_credentials_still_generate_fingerprint() {
    let params = pc_app_params_with(None);
    assert!(!params.get("device_id").unwrap_or("").is_empty());
    assert!(!params.get("fp").unwrap_or("").is_empty());
    // 无凭证的 URL 与历史行为一致（每次现生成设备号）
    assert!(pc_track_v2_url().contains("device_id="));
}

#[test]
fn credentials_load_from_file() {
    let dir = std::env::temp_dir().join(format!("libresoda-creds-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("credentials.json");
    std::fs::write(
        &path,
        r#"{"device_id":"7000000000000001","x_helios":"h","x_medusa":"m"}"#,
    )
    .expect("write credentials");

    let soda = Soda::new("sessionid_ss=test");
    soda.load_app_credentials(&path).expect("load credentials");
    let credentials = soda.app_credentials().expect("credentials present");
    assert!(credentials.is_complete());
    assert_eq!(credentials.fp_or_device_id(), "7000000000000001");
    assert_eq!(soda.app_signature_headers().len(), 2);

    drop(std::fs::remove_file(&path));
    drop(std::fs::remove_dir(&dir));
}

#[test]
fn download_info_marks_preview_by_default() {
    let info = libresoda::soda::DownloadInfo::default();
    assert!(!info.is_preview);
    assert!(info.note.is_empty());
}
