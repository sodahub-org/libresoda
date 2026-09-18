//! 扫码登录（按 Meting-API 复刻）离线测试。

use libresoda::soda::qr_login::{base64_url_no_pad_for_test, md5_hex_upper, official_scan_url};

#[test]
fn md5_matches_rfc1321_vectors() {
    assert_eq!(md5_hex_upper(b""), "D41D8CD98F00B204E9800998ECF8427E");
    assert_eq!(md5_hex_upper(b"abc"), "900150983CD24FB0D6963F7D28E17F72");
    assert_eq!(
        md5_hex_upper(b"The quick brown fox jumps over the lazy dog"),
        "9E107D9D372BB6826BD81D3542A419D6"
    );
}

#[test]
fn base64url_is_unpadded() {
    assert_eq!(base64_url_no_pad_for_test(b"f"), "Zg");
    assert_eq!(base64_url_no_pad_for_test(b"fo"), "Zm8");
    assert_eq!(base64_url_no_pad_for_test(b"foo"), "Zm9v");
    assert_eq!(base64_url_no_pad_for_test(&[0xfb, 0xff]), "-_8");
}

#[test]
fn scan_url_keeps_official_index_url() {
    // 实测结论：二维码内容必须是服务端下发的 qrcode_index_url 原样。
    // 改写成 light/invoke/scan_login 只会让手机上报「已扫码」，确认永远落不了地。
    let index = "https://bff-pc.qishui.com/ucenter_web/app/sdk-next?device_platform=PC&next_url=x&qr_source_aid=386088&token=deadbeef_lf&uc_sdk=scan-auth";
    let url = official_scan_url(index).expect("scan url");
    assert_eq!(url, index);
}

#[test]
fn scan_url_requires_token() {
    let err = official_scan_url("https://example.com/no-token").expect_err("should fail");
    assert!(err.to_string().contains("token"), "{err}");
    let err = official_scan_url("   ").expect_err("should fail");
    assert!(err.to_string().contains("扫码地址"), "{err}");
}
