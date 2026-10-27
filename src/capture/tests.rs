use super::*;

const HAR: &str = r#"{
  "log": {
    "version": "1.2",
    "creator": { "name": "Chrome", "version": "120" },
    "entries": [
      {
        "startedDateTime": "2026-10-27T10:00:00.000Z",
        "time": 123.4,
        "serverIPAddress": "10.0.0.5",
        "request": {
          "method": "GET",
          "url": "https://api.example.com/v1/users?page=2",
          "headers": [
            { "name": "Accept", "value": "application/json" },
            { "name": "Content-Type", "value": "text/plain" }
          ]
        },
        "response": {
          "status": 200,
          "statusText": "OK",
          "headers": [{ "name": "Content-Type", "value": "application/json" }],
          "content": {
            "size": 29,
            "mimeType": "application/json",
            "text": "{\"items\":[],\"total\":0}\r\n"
          }
        }
      },
      {
        "startedDateTime": "2026-10-27T10:00:01.000Z",
        "time": 80,
        "request": {
          "method": "POST",
          "url": "https://api.example.com/v1/login",
          "headers": [],
          "postData": {
            "mimeType": "application/x-www-form-urlencoded",
            "params": [
              { "name": "user", "value": "alice" },
              { "name": "token", "value": "x%20y" }
            ]
          }
        },
        "response": {
          "status": 500,
          "headers": [{ "name": "Content-Type", "value": "text/plain" }],
          "content": {
            "size": 4,
            "mimeType": "text/plain",
            "text": "Ym9vbQ==",
            "encoding": "base64"
          }
        }
      }
    ]
  }
}"#;

const CHARLES_JSON: &str = r#"[
  {
    "host": "httpbin.org",
    "scheme": "https",
    "port": 443,
    "path": "/get",
    "query": "a=1&b=2",
    "method": "GET",
    "protocolVersion": "HTTP/1.1",
    "status": "COMPLETE",
    "request": {
      "header": { "headers": [{ "name": "Host", "value": "httpbin.org" }] },
      "sizes": { "body": 0 }
    },
    "response": {
      "status": 200,
      "header": { "headers": [{ "name": "Content-Type", "value": "application/json" }] },
      "body": { "text": "{\"ok\":true}", "mimeType": "application/json" },
      "sizes": { "body": 11 }
    },
    "times": { "start": "2026-10-27T10:00:00.500+08:00" },
    "durations": { "total": 45.5 },
    "remoteAddress": "203.0.113.7"
  },
  {
    "host": "api.example.com",
    "scheme": "https",
    "actualPort": 8443,
    "path": "/broken",
    "method": "GET",
    "status": "FAILED",
    "errorMessage": "Connection refused",
    "request": { "header": { "headers": [] } },
    "times": { "start": "2026-10-27T10:00:02.000+08:00" }
  }
]"#;

const CHARLES_XML: &str = r#"<?xml version='1.0' encoding='UTF-8' ?>
<cha:charles-session xmlns:cha="http://www.charlesproxy.com/2010/schema">
  <transaction method="POST" host="api.example.com" port="443" protocol="https" path="/v1/echo" query="x=1" status="COMPLETE" startTime="2026-10-27T10:00:00.000Z" startTimeMillis="1000" responseTimeMillis="1120" endTimeMillis="1130" remoteAddress="198.51.100.4">
    <request>
      <headers>
        <first-line>POST /v1/echo?x=1 HTTP/1.1</first-line>
        <header><name>Content-Type</name><value>application/json</value></header>
        <header><name>X-Note</name><value>a &amp; b</value></header>
      </headers>
      <body>{"msg":"hi &amp; bye"}</body>
    </request>
    <response status="201" mime-type="application/json">
      <headers>
        <first-line>HTTP/1.1 201 Created</first-line>
        <header><name>Content-Type</name><value>application/json</value></header>
      </headers>
      <body encoding="base64">eyJvayI6dHJ1ZX0=</body>
    </response>
  </transaction>
  <transaction method="GET" host="cdn.example.com" port="80" protocol="http" path="/img.png" status="COMPLETE" startTimeMillis="2000" responseTimeMillis="2100">
    <request><headers/><body/></request>
    <response status="200" mime-type="image/png"><headers/><body encoding="base64">iVBORw0KGgo=</body></response>
  </transaction>
</cha:charles-session>"#;

fn parse(name: &str, body: &str) -> CaptureLog {
    parse_bytes(name, body.as_bytes()).expect("fixture should parse")
}

#[test]
fn har_entries_keep_url_status_headers_and_bodies() {
    let log = parse("browser.har", HAR);
    assert_eq!(log.format, CaptureFormat::Har);
    assert_eq!(log.file_name, "browser.har");
    assert!(!log.truncated);
    assert_eq!(log.entries.len(), 2);

    let get = &log.entries[0];
    assert_eq!(get.method, "GET");
    assert_eq!(get.host, "api.example.com");
    assert_eq!(get.path, "/v1/users", "列表只显示路径");
    assert_eq!(get.query.as_deref(), Some("page=2"));
    assert_eq!(get.status, Some(200));
    assert_eq!(get.duration_ms, Some(123.4));
    assert_eq!(get.mime, "application/json");
    assert_eq!(get.remote_address.as_deref(), Some("10.0.0.5"));
    assert_eq!(get.response_headers[0].name, "Content-Type");
    assert_eq!(
        get.response_body
            .as_ref()
            .and_then(|body| body.preview.as_deref()),
        Some("{\"items\":[],\"total\":0}\r\n")
    );

    let post = &log.entries[1];
    assert_eq!(post.method, "POST");
    assert_eq!(post.status, Some(500));
    assert_eq!(
        post.request_body
            .as_ref()
            .and_then(|body| body.preview.as_deref()),
        Some("user=alice&token=x%20y"),
        "postData.params 要还原成可读的表单串"
    );
    let response = post.response_body.as_ref().expect("base64 body");
    assert_eq!(response.preview.as_deref(), Some("boom"));
    assert_eq!(response.size, Some(4));
    assert!(!response.binary);
}

#[test]
fn charles_json_builds_urls_and_marks_failures() {
    let log = parse("session.chlsj", CHARLES_JSON);
    assert_eq!(log.format, CaptureFormat::CharlesJson);
    assert_eq!(log.entries.len(), 2);

    let first = &log.entries[0];
    assert_eq!(first.url, "https://httpbin.org/get?a=1&b=2");
    assert_eq!(first.path, "/get");
    assert_eq!(first.query.as_deref(), Some("a=1&b=2"));
    assert_eq!(first.status, Some(200));
    assert_eq!(first.duration_ms, Some(45.5));
    assert_eq!(first.size, Some(11));
    assert_eq!(
        first
            .response_body
            .as_ref()
            .and_then(|body| body.preview.as_deref()),
        Some("{\"ok\":true}")
    );

    let failed = &log.entries[1];
    assert_eq!(failed.url, "https://api.example.com:8443/broken");
    assert_eq!(failed.status, None, "失败请求没有可展示的状态码");
    assert!(
        failed
            .note
            .as_deref()
            .is_some_and(|note| note.contains("Connection refused")),
        "失败原因要保留：{:?}",
        failed.note
    );
}

#[test]
fn charles_xml_handles_namespaces_entities_and_base64() {
    let log = parse("session.chls", CHARLES_XML);
    assert_eq!(log.format, CaptureFormat::CharlesXml);
    assert_eq!(log.entries.len(), 2);

    let echo = &log.entries[0];
    assert_eq!(echo.method, "POST");
    assert_eq!(echo.url, "https://api.example.com/v1/echo?x=1");
    assert_eq!(echo.path, "/v1/echo");
    assert_eq!(echo.query.as_deref(), Some("x=1"));
    assert_eq!(echo.status, Some(201));
    assert_eq!(echo.mime, "application/json");
    assert_eq!(echo.duration_ms, Some(130.0));
    assert_eq!(echo.request_headers.len(), 2);
    assert_eq!(echo.request_headers[1].name, "X-Note");
    assert_eq!(echo.request_headers[1].value, "a & b");
    assert_eq!(
        echo.request_body
            .as_ref()
            .and_then(|body| body.preview.as_deref()),
        Some("{\"msg\":\"hi & bye\"}"),
        "XML 实体引用要还原成原字符"
    );
    assert_eq!(
        echo.response_body
            .as_ref()
            .and_then(|body| body.preview.as_deref()),
        Some("{\"ok\":true}")
    );

    let image = &log.entries[1];
    assert_eq!(
        image.url, "http://cdn.example.com/img.png",
        "默认端口不进 URL"
    );
    let body = image.response_body.as_ref().expect("png body");
    assert!(body.binary, "PNG 字节不是 UTF-8，应标记为二进制");
    assert_eq!(body.preview, None);
    assert_eq!(body.size, Some(8));
    assert_eq!(body.mime, "image/png");
    assert!(image.request_body.is_none(), "空 body 不生成正文");
}

#[test]
fn rejects_unknown_shapes_with_actionable_errors() {
    let error = parse_bytes("x.json", br#"{"hello":1}"#).unwrap_err();
    assert!(error.contains("HAR"), "{error}");
    let error = parse_bytes("x.xml", b"<foo/>").unwrap_err();
    assert!(error.contains("charles-session"), "{error}");
    let error = parse_bytes("x.txt", b"hello").unwrap_err();
    assert!(error.contains("HAR"), "{error}");
}

#[test]
fn utf8_bom_is_ignored_for_both_encodings() {
    let har = format!("\u{feff}{HAR}");
    let log = parse_bytes("bom.har", har.as_bytes()).expect("HAR with BOM");
    assert_eq!(log.entries.len(), 2);
    let xml = format!("\u{feff}{CHARLES_XML}");
    let log = parse_bytes("bom.chls", xml.as_bytes()).expect("XML with BOM");
    assert_eq!(log.entries.len(), 2);
}

#[test]
fn query_strings_decode_keys_values_and_fragments() {
    assert_eq!(
        parse_query("q=hello+world&path=%2Fapi%2Fv1&flag&empty="),
        vec![
            ("q".to_string(), "hello world".to_string()),
            ("path".to_string(), "/api/v1".to_string()),
            ("flag".to_string(), String::new()),
            ("empty".to_string(), String::new()),
        ]
    );
    assert!(parse_query("").is_empty());
    assert_eq!(
        parse_query("bad=%zz&ok=1"),
        vec![
            ("bad".to_string(), "%zz".to_string()),
            ("ok".to_string(), "1".to_string()),
        ],
        "残缺的百分号编码原样保留，不吞字符"
    );
    assert_eq!(
        split_url("https://api.example.com/a/b?x=1#top"),
        (
            "api.example.com".to_string(),
            "/a/b".to_string(),
            Some("x=1".to_string())
        )
    );
    assert_eq!(
        split_url("https://api.example.com"),
        ("api.example.com".to_string(), "/".to_string(), None)
    );
}

#[test]
fn status_classes_cover_every_family_and_gap() {
    assert_eq!(status_class(Some(204)), StatusClass::Success);
    assert_eq!(status_class(Some(301)), StatusClass::Redirect);
    assert_eq!(status_class(Some(404)), StatusClass::ClientError);
    assert_eq!(status_class(Some(503)), StatusClass::ServerError);
    assert_eq!(status_class(Some(100)), StatusClass::Pending);
    assert_eq!(status_class(Some(0)), StatusClass::Pending);
    assert_eq!(status_class(None), StatusClass::Pending);
}

#[test]
fn entry_cap_reports_truncation_instead_of_silently_dropping() {
    let (entries, truncated, warnings) = squash(Vec::new(), MAX_ENTRIES + 1, Vec::new());
    assert!(truncated);
    assert!(entries.is_empty());
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("only the first"), "{}", warnings[0]);
}
