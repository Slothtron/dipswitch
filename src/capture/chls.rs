//! Charles 二进制会话（.chls 是 Java 对象序列化流）。
//!
//! Charles 的「Session File」不是 XML/JSON，而是 `ObjectOutputStream` 写出的
//! 对象图：`Session → ModelNode.children → Host/Path → Transaction`。本模块用
//! `java-serialization` 把流读成自描述对象，再按类名与字段名取出请求；字段名
//! 来自流里的类描述，不依赖 Charles 的版本号。
//!
//! 只读取展示需要的部分：URL、状态、头部、正文与时间。正文沿用与其他格式相同的
//! 预览预算，二进制正文只标大小。

use std::collections::HashMap;
use std::io::Read;

use flate2::read::{GzDecoder, ZlibDecoder};
use java_serialization::{
    AnnotationElement, ArrayValues, ClassData, ClassDesc, ClassDescRef, ContentElement, FieldDesc,
    FieldValue, NewObject, NormalClassDesc, ObjectAnnotation, SerializationStream, StreamObject,
    parse_serialization_stream,
};

use super::{
    BODY_PREVIEW_BYTES, Body, BodyBudget, CaptureFormat, CaptureLog, Entry, Header, MAX_ENTRIES,
    squash, strip_query, truncate_chars,
};

type DescMap<'a> = HashMap<u32, &'a ClassDesc>;
type ObjMap<'a> = HashMap<u32, &'a StreamObject>;

/// Java 序列化流的魔数：`AC ED` + 版本 5。
pub const MAGIC: [u8; 2] = [0xAC, 0xED];

/// 解压后的正文预览上限（防压缩炸弹）；超过就只标记大小。
const MAX_DECOMPRESSED_BYTES: usize = 64 * 1024 * 1024;

pub fn parse(file_name: &str, bytes: &[u8]) -> Result<CaptureLog, String> {
    let (rest, stream) = parse_serialization_stream(bytes)
        .map_err(|error| format!("Could not parse the Charles binary session: {error:?}"))?;
    if !rest.is_empty() {
        return Err("Charles binary session has trailing data".to_string());
    }

    let graph = Graph::new(&stream);
    let session = stream
        .objects()
        .find_map(|object| match object {
            StreamObject::NewObject(object)
                if graph.object_class_name(object) == Some("com.xk72.charles.model.Session") =>
            {
                Some(object)
            }
            _ => None,
        })
        .ok_or_else(|| "Not a Charles session: no com.xk72.charles.model.Session".to_string())?;

    let hosts = graph
        .field_object(session, "children")
        .map(|children| graph.list(children))
        .unwrap_or_default();
    let mut transactions: Vec<&NewObject> = Vec::new();
    let mut stack: Vec<&StreamObject> = hosts;
    while let Some(object) = stack.pop() {
        let StreamObject::NewObject(object) = object else {
            continue;
        };
        let name = graph.object_class_name(object).unwrap_or("");
        if name.ends_with(".Transaction") {
            transactions.push(object);
            continue;
        }
        if let Some(children) = graph.field_object(object, "children") {
            stack.extend(graph.list(children));
        }
    }
    transactions.sort_by_key(|transaction| {
        graph
            .field_object(transaction, "startTime")
            .and_then(|start| graph.date_ms(start))
            .unwrap_or(i64::MAX)
    });

    let total = transactions.len();
    let mut budget = BodyBudget::new();
    let mut entries = Vec::with_capacity(total.min(MAX_ENTRIES));
    for transaction in transactions.into_iter().take(MAX_ENTRIES) {
        entries.push(entry_for(&graph, transaction, &mut budget));
    }
    let (entries, truncated, warnings) = squash(entries, total, Vec::new());
    Ok(CaptureLog {
        format: CaptureFormat::CharlesBinary,
        file_name: file_name.to_string(),
        entries,
        truncated,
        warnings,
    })
}

struct Graph<'a> {
    descs: DescMap<'a>,
    objects: ObjMap<'a>,
}

impl<'a> Graph<'a> {
    fn new(stream: &'a SerializationStream) -> Self {
        let mut graph = Self {
            descs: HashMap::new(),
            objects: HashMap::new(),
        };
        for content in &stream.contents {
            if let ContentElement::Object(object) = content {
                graph.collect(object);
            }
        }
        graph
    }

    fn collect(&mut self, object: &'a StreamObject) {
        // PrevObject 只是指针，不能覆盖它指向的实体。
        if !matches!(object, StreamObject::PrevObject { .. })
            && let Some(handle) = object.handle()
        {
            self.objects.insert(handle, object);
        }
        match object {
            StreamObject::NewObject(object) => {
                self.collect_desc(&object.class_desc);
                for class_data in &object.class_data {
                    match class_data {
                        ClassData::NoWriteMethod(values) => self.collect_values(&values.values),
                        ClassData::WriteMethodWithFields(values, annotation) => {
                            self.collect_values(&values.values);
                            self.collect_annotation(annotation);
                        }
                        ClassData::WriteMethod(annotation)
                        | ClassData::ExternalBlockData(annotation) => {
                            self.collect_annotation(annotation)
                        }
                        ClassData::ExternalContents(_) => {}
                    }
                }
            }
            StreamObject::NewArray(array) => {
                self.collect_desc(&array.class_desc);
                if let ArrayValues::Object(values) = &array.values {
                    for value in values.iter().flatten() {
                        self.collect(value);
                    }
                }
            }
            StreamObject::NewEnum(enumeration) => self.collect_desc(&enumeration.class_desc),
            _ => {}
        }
    }

    fn collect_desc(&mut self, desc: &'a ClassDescRef) {
        if let ClassDescRef::Inline(desc) = desc
            && let ClassDesc::Normal(normal) = desc.as_ref()
        {
            self.descs.insert(normal.handle, desc);
            self.collect_desc(&normal.super_class_desc);
        }
    }

    fn collect_values(&mut self, values: &'a [FieldValue]) {
        for value in values {
            if let FieldValue::Object(Some(object)) = value {
                self.collect(object);
            }
        }
    }

    fn collect_annotation(&mut self, annotation: &'a ObjectAnnotation) {
        for element in &annotation.contents {
            if let AnnotationElement::Object(object) = element {
                self.collect(object);
            }
        }
    }

    fn resolve(&self, object: &'a StreamObject) -> &'a StreamObject {
        let mut current = object;
        for _ in 0..8 {
            match current {
                StreamObject::PrevObject { handle } => match self.objects.get(handle) {
                    Some(next) => current = next,
                    None => return current,
                },
                other => return other,
            }
        }
        current
    }

    fn object_class_name(&self, object: &'a NewObject) -> Option<&'a str> {
        self.desc_name(&object.class_desc)
    }

    fn desc_name(&self, desc: &'a ClassDescRef) -> Option<&'a str> {
        match desc {
            ClassDescRef::Inline(desc) => desc.class_name(),
            ClassDescRef::Reference { handle } => {
                self.descs.get(handle).and_then(|desc| desc.class_name())
            }
            ClassDescRef::Null => None,
        }
    }

    fn normal(&self, desc: &'a ClassDescRef) -> Option<&'a NormalClassDesc> {
        match desc {
            ClassDescRef::Inline(desc) => match desc.as_ref() {
                ClassDesc::Normal(normal) => Some(normal),
                ClassDesc::Proxy(_) => None,
            },
            ClassDescRef::Reference { handle } => match self.descs.get(handle)? {
                ClassDesc::Normal(normal) => Some(normal),
                ClassDesc::Proxy(_) => None,
            },
            ClassDescRef::Null => None,
        }
    }

    fn hierarchy(&self, desc: &'a ClassDescRef) -> Vec<&'a NormalClassDesc> {
        let mut levels = Vec::new();
        let mut cursor = self.normal(desc);
        while let Some(level) = cursor {
            levels.push(level);
            cursor = self.normal(&level.super_class_desc);
        }
        levels.reverse();
        levels
    }

    fn fields(&self, object: &'a NewObject) -> Vec<(&'a str, &'a FieldValue)> {
        let levels = self.hierarchy(&object.class_desc);
        let mut out = Vec::new();
        for (index, class_data) in object.class_data.iter().enumerate() {
            let Some(level) = levels.get(index) else {
                continue;
            };
            let values = match class_data {
                ClassData::NoWriteMethod(values) | ClassData::WriteMethodWithFields(values, _) => {
                    &values.values
                }
                _ => continue,
            };
            for (field, value) in level.fields.iter().zip(values.iter()) {
                let name = match field {
                    FieldDesc::Primitive(field) => field.field_name.as_str(),
                    FieldDesc::Object(field) => field.field_name.as_str(),
                };
                out.push((name, value));
            }
        }
        out
    }

    fn field(&self, object: &'a NewObject, name: &str) -> Option<&'a FieldValue> {
        self.fields(object)
            .into_iter()
            .find(|(field, _)| *field == name)
            .map(|(_, value)| value)
    }

    fn field_object(&self, object: &'a NewObject, name: &str) -> Option<&'a StreamObject> {
        match self.field(object, name)? {
            FieldValue::Object(Some(object)) => Some(self.resolve(object)),
            _ => None,
        }
    }

    fn string_field(&self, object: &'a NewObject, name: &str) -> Option<&'a str> {
        self.string(self.field(object, name)?)
    }

    fn integer_field(&self, object: &'a NewObject, name: &str) -> Option<i64> {
        match self.field(object, name)? {
            FieldValue::Byte(value) => Some(*value as i64),
            FieldValue::Short(value) => Some(*value as i64),
            FieldValue::Int(value) => Some(*value as i64),
            FieldValue::Long(value) => Some(*value),
            _ => None,
        }
    }

    fn string(&self, value: &'a FieldValue) -> Option<&'a str> {
        match value {
            FieldValue::Object(Some(object)) => match self.resolve(object) {
                StreamObject::NewString(string) => Some(&string.value),
                _ => None,
            },
            _ => None,
        }
    }

    /// `ArrayList` 的 `writeObject` 把元素写在注解里：4 字节长度 + 元素对象。
    fn list(&self, object: &'a StreamObject) -> Vec<&'a StreamObject> {
        let object = self.resolve(object);
        match object {
            StreamObject::NewArray(array) => match &array.values {
                ArrayValues::Object(values) => values
                    .iter()
                    .flatten()
                    .map(|value| self.resolve(value))
                    .collect(),
                _ => Vec::new(),
            },
            StreamObject::NewObject(list) => {
                if self
                    .object_class_name(list)
                    .is_some_and(|name| name.ends_with("EmptyList"))
                {
                    return Vec::new();
                }
                if let Some(array) = self.field_object(list, "a") {
                    return self.list(array);
                }
                for class_data in &list.class_data {
                    let annotation = match class_data {
                        ClassData::WriteMethod(annotation)
                        | ClassData::WriteMethodWithFields(_, annotation) => annotation,
                        _ => continue,
                    };
                    let items: Vec<&StreamObject> = annotation
                        .contents
                        .iter()
                        .filter_map(|element| match element {
                            AnnotationElement::Object(object) => Some(self.resolve(object)),
                            AnnotationElement::BlockData(_) => None,
                        })
                        .collect();
                    if !items.is_empty() {
                        return items;
                    }
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn list_strings(&self, object: &'a StreamObject) -> Vec<&'a str> {
        self.list(object)
            .into_iter()
            .filter_map(|object| match object {
                StreamObject::NewString(string) => Some(string.value.as_str()),
                _ => None,
            })
            .collect()
    }

    fn byte_slice(&self, object: &'a StreamObject) -> Option<&'a [i8]> {
        match self.resolve(object) {
            StreamObject::NewArray(array) => match &array.values {
                ArrayValues::Byte(values) => Some(values.as_slice()),
                _ => None,
            },
            _ => None,
        }
    }

    fn date_ms(&self, object: &'a StreamObject) -> Option<i64> {
        let StreamObject::NewObject(date) = self.resolve(object) else {
            return None;
        };
        for class_data in &date.class_data {
            let annotation = match class_data {
                ClassData::WriteMethod(annotation)
                | ClassData::WriteMethodWithFields(_, annotation) => annotation,
                _ => continue,
            };
            for element in &annotation.contents {
                if let AnnotationElement::BlockData(block) = element
                    && let [a, b, c, d, e, f, g, h] = block.data()
                {
                    return Some(i64::from_be_bytes([*a, *b, *c, *d, *e, *f, *g, *h]));
                }
            }
        }
        None
    }

    fn annotation_strings(&self, object: &'a NewObject) -> Vec<&'a str> {
        let mut out = Vec::new();
        for class_data in &object.class_data {
            let annotation = match class_data {
                ClassData::WriteMethod(annotation)
                | ClassData::WriteMethodWithFields(_, annotation) => annotation,
                _ => continue,
            };
            for element in &annotation.contents {
                if let AnnotationElement::Object(object) = element
                    && let StreamObject::NewString(string) = self.resolve(object)
                {
                    out.push(string.value.as_str());
                }
            }
        }
        out
    }

    fn inet_address(&self, object: &'a StreamObject) -> Option<String> {
        let StreamObject::NewObject(address) = self.resolve(object) else {
            return None;
        };
        let family = self.integer_field(address, "family");
        let raw = self.integer_field(address, "address");
        if family == Some(2)
            && let Some(raw) = raw
        {
            let bytes = (raw as i32).to_be_bytes();
            return Some(format!(
                "{}.{}.{}.{}",
                bytes[0], bytes[1], bytes[2], bytes[3]
            ));
        }
        self.string_field(address, "hostName").map(str::to_string)
    }
}

fn entry_for(graph: &Graph<'_>, transaction: &NewObject, budget: &mut BodyBudget) -> Entry {
    let request_header = graph.field_object(transaction, "requestHeader");
    let response_header = graph.field_object(transaction, "responseHeader");
    let (request_headers, request_line) = headers(graph, request_header);
    let (response_headers, response_line) = headers(graph, response_header);

    let method = header_value(&request_headers, ":method")
        .map(str::to_string)
        .or_else(|| {
            graph
                .string_field(transaction, "scheme")
                .map(str::to_string)
        })
        .or_else(|| {
            request_line
                .as_deref()
                .and_then(|line| line.split_whitespace().next())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "GET".to_string());
    let status = header_value(&response_headers, ":status")
        .and_then(|value| value.parse::<u16>().ok())
        .or_else(|| response_line.as_deref().and_then(status_from_line));
    let protocol = graph
        .string_field(transaction, "protocol")
        .filter(|protocol| !protocol.is_empty())
        .unwrap_or("http");
    let host = graph.string_field(transaction, "host").unwrap_or("");
    let file = graph
        .string_field(transaction, "file")
        .filter(|file| !file.is_empty())
        .unwrap_or("/");
    let port = graph.integer_field(transaction, "actualPort");
    let (path, query) = strip_query(file);
    let mut url = format!("{protocol}://{host}");
    if let Some(port) = port.filter(|port| !is_default_port(protocol, *port)) {
        url.push(':');
        url.push_str(&port.to_string());
    }
    url.push_str(&path);
    if let Some(query) = query.as_deref() {
        url.push('?');
        url.push_str(query);
    }

    let mime = header_value(&response_headers, "content-type")
        .map(mime_label)
        .unwrap_or_default();
    let request_mime = header_value(&request_headers, "content-type")
        .map(mime_label)
        .unwrap_or_default();
    let response_bytes = graph
        .field_object(transaction, "responseBody")
        .and_then(|body| graph.byte_slice(body));
    let request_bytes = graph
        .field_object(transaction, "requestBody")
        .and_then(|body| graph.byte_slice(body));
    let response_encoding = header_value(&response_headers, "content-encoding");
    let request_encoding = header_value(&request_headers, "content-encoding");
    let response_body =
        response_bytes.and_then(|bytes| java_body(bytes, &mime, response_encoding, budget));
    let request_body =
        request_bytes.and_then(|bytes| java_body(bytes, &request_mime, request_encoding, budget));
    let size = graph
        .integer_field(transaction, "responseSizeLong")
        .filter(|size| *size >= 0)
        .map(|size| size as u64)
        .or_else(|| response_body.as_ref().and_then(|body| body.size));
    let started_ms = graph
        .field_object(transaction, "startTime")
        .and_then(|start| graph.date_ms(start));
    let ended_ms = graph
        .field_object(transaction, "endTime")
        .and_then(|end| graph.date_ms(end));
    // Charles 的 Duration 从「请求开始写出」量到响应结束，不含建连那 1ms；
    // 用事务起止会比它的读数多 1。
    let request_started_ms = graph
        .field_object(transaction, "requestBeginTime")
        .and_then(|start| graph.date_ms(start));
    let duration_ms = match (request_started_ms.or(started_ms), ended_ms) {
        (Some(start), Some(end)) if end >= start => Some((end - start) as f64),
        _ => None,
    };
    let exception = graph
        .field_object(transaction, "exception")
        .filter(|exception| !matches!(exception, StreamObject::NullReference));
    let note = exception.map(|exception| {
        let message = match exception {
            StreamObject::NewObject(exception) => graph
                .string_field(exception, "detailMessage")
                .filter(|message| !message.is_empty())
                .map(str::to_string),
            _ => None,
        };
        match message {
            Some(message) => format!("Charles: {message}"),
            None => "Charles: request failed".to_string(),
        }
    });
    Entry {
        method,
        host: host.to_string(),
        path,
        query,
        url,
        status: if note.is_some() { None } else { status },
        note,
        protocol: None,
        mime,
        size,
        duration_ms,
        started: started_ms.map(iso_from_ms),
        remote_address: graph
            .field_object(transaction, "remoteAddress")
            .and_then(|address| graph.inet_address(address)),
        request_headers,
        response_headers,
        request_body,
        response_body,
    }
}

fn headers(graph: &Graph<'_>, header: Option<&StreamObject>) -> (Vec<Header>, Option<String>) {
    let Some(StreamObject::NewObject(header)) = header else {
        return (Vec::new(), None);
    };
    let names = graph
        .field_object(header, "fieldNames")
        .map(|names| graph.list_strings(names))
        .unwrap_or_default();
    let values = graph
        .field_object(header, "fieldValues")
        .map(|values| graph.list_strings(values))
        .unwrap_or_default();
    let headers = names
        .iter()
        .zip(values.iter())
        .map(|(name, value)| Header {
            name: (*name).to_string(),
            value: (*value).to_string(),
        })
        .collect();
    let first_line = graph
        .annotation_strings(header)
        .into_iter()
        .find(|line| line.starts_with("HTTP/") || line.contains(" HTTP/"))
        .map(str::to_string);
    (headers, first_line)
}

fn header_value<'a>(headers: &'a [Header], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str())
}

fn status_from_line(line: &str) -> Option<u16> {
    line.split_whitespace().nth(1)?.parse().ok()
}

fn mime_label(value: &str) -> String {
    value.split(';').next().unwrap_or(value).trim().to_string()
}

fn is_default_port(protocol: &str, port: i64) -> bool {
    (protocol == "http" && port == 80) || (protocol == "https" && port == 443)
}

fn java_body(
    bytes: &[i8],
    mime: &str,
    encoding: Option<&str>,
    budget: &mut BodyBudget,
) -> Option<Body> {
    if bytes.is_empty() {
        return None;
    }
    let wire_len = bytes.len();
    let raw: Vec<u8> = bytes.iter().map(|byte| *byte as u8).collect();
    let raw = match decode_content_encoding(&raw, encoding) {
        Ok(raw) => raw,
        // 解不开就留着线上字节：至少 Hex 视图还能看出这是什么内容。
        Err(()) => {
            let prefix = &raw[..raw.len().min(BODY_PREVIEW_BYTES)];
            return Some(binary_body(prefix, wire_len, mime));
        }
    };
    let len = raw.len();
    let preview_len = len.min(BODY_PREVIEW_BYTES).min(budget.remaining);
    let prefix = &raw[..preview_len];
    let valid = match std::str::from_utf8(prefix) {
        Ok(_) => prefix.len(),
        // 只有「我们自己把多字节字符截断」时才接受前缀；正文里出现非法字节
        // 说明它根本不是 UTF-8（例如压缩数据），按二进制处理。
        Err(error) if preview_len < len && error.error_len().is_none() => error.valid_up_to(),
        Err(_) => return Some(binary_body(prefix, len, mime)),
    };
    if valid == 0 {
        return Some(binary_body(prefix, len, mime));
    }
    let text = std::str::from_utf8(&prefix[..valid]).ok()?;
    let allowed = budget.take(text.len());
    Some(Body {
        preview: Some(truncate_chars(text, allowed)),
        size: Some(len as u64),
        truncated: allowed < len,
        binary: false,
        bytes: None,
        mime: mime.to_string(),
    })
}

/// 二进制正文：没有文本预览，但原始字节留着，Hex 视图靠它。
fn binary_body(prefix: &[u8], size: usize, mime: &str) -> Body {
    Body {
        preview: None,
        size: Some(size as u64),
        truncated: prefix.len() < size,
        binary: true,
        bytes: Some(prefix.to_vec()),
        mime: mime.to_string(),
    }
}

/// 按 `Content-Encoding` 解压正文。没有编码原样返回；编码认得但解不开时
/// 交给调用方按二进制显示，不把乱码当文本。
fn decode_content_encoding(bytes: &[u8], encoding: Option<&str>) -> Result<Vec<u8>, ()> {
    let Some(encoding) = encoding else {
        return Ok(bytes.to_vec());
    };
    let encoding = encoding.to_ascii_lowercase();
    let reader: Box<dyn Read> = if encoding.contains("gzip") {
        Box::new(GzDecoder::new(bytes))
    } else if encoding.contains("deflate") {
        Box::new(ZlibDecoder::new(bytes))
    } else {
        return Ok(bytes.to_vec());
    };
    let mut decoded = Vec::new();
    reader
        .take(MAX_DECOMPRESSED_BYTES as u64 + 1)
        .read_to_end(&mut decoded)
        .map_err(|_| ())?;
    if decoded.len() > MAX_DECOMPRESSED_BYTES {
        return Err(());
    }
    Ok(decoded)
}

/// 会话里的 `java.util.Date` 是绝对时刻，这里统一写成 UTC 的 RFC 3339；
/// 界面再按本机时区渲染，四种格式共用同一条展示规则。
fn iso_from_ms(ms: i64) -> String {
    // Howard Hinnant 的 civil_from_days：把 Unix 天数换成公历年月日。
    let seconds = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000) as u32;
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400) as u32;
    let (year, month, day) = civil_from_days(days);
    let hour = day_seconds / 3600;
    let minute = day_seconds % 3600 / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_and_deflate_bodies_are_decoded() {
        use std::io::Write;

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"<html>ok</html>").expect("write gzip");
        let gzipped = encoder.finish().expect("finish gzip");
        assert_eq!(
            decode_content_encoding(&gzipped, Some("gzip")).expect("gzip decodes"),
            b"<html>ok</html>"
        );

        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(b"hello").expect("write deflate");
        let deflated = encoder.finish().expect("finish deflate");
        assert_eq!(
            decode_content_encoding(&deflated, Some("deflate")).expect("deflate decodes"),
            b"hello"
        );

        assert!(
            decode_content_encoding(&[0x1f, 0x8b, 0xff], Some("gzip")).is_err(),
            "标注了 gzip 但解不开时要交给二进制分支"
        );
        assert_eq!(
            decode_content_encoding(&[1, 2, 3], None).expect("no encoding"),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn invalid_utf8_is_binary_instead_of_a_blank_preview() {
        // gzip 魔数没有 Content-Encoding 时也不能当文本：0x1f 后紧跟非法字节。
        let bytes = [0x1f_u8 as i8, 0x8b_u8 as i8, 0x08, 0x00];
        let mut budget = BodyBudget::new();
        let body = java_body(&bytes, "text/html", None, &mut budget).expect("body");
        assert!(body.binary);
        assert_eq!(body.preview, None);
        assert_eq!(body.size, Some(4));
        assert_eq!(
            body.bytes.as_deref(),
            Some(&[0x1f_u8, 0x8b, 0x08, 0x00][..]),
            "二进制正文要留下原始字节，Hex 视图靠它"
        );
    }

    #[test]
    fn iso_formatting_matches_known_epochs() {
        assert_eq!(iso_from_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_ms(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
    }

    #[test]
    fn synthetic_session_loads_two_transactions() {
        let bytes = include_bytes!("fixtures/minimal.chls");
        let log = parse("minimal.chls", bytes).expect("fixture parses");
        assert_eq!(log.format, CaptureFormat::CharlesBinary);
        assert_eq!(log.entries.len(), 2);
        let first = &log.entries[0];
        assert_eq!(first.method, "GET");
        assert_eq!(first.url, "https://api.example.com/v1/items?page=2");
        assert_eq!(first.path, "/v1/items");
        assert_eq!(first.query.as_deref(), Some("page=2"));
        assert_eq!(first.status, Some(200));
        assert_eq!(first.mime, "application/json");
        assert_eq!(
            first
                .response_body
                .as_ref()
                .and_then(|body| body.preview.as_deref()),
            Some("{\"items\":[],\"total\":0}")
        );
        assert_eq!(first.duration_ms, Some(123.0));
        assert_eq!(first.remote_address.as_deref(), Some("203.0.113.9"));
        assert_eq!(
            header_value(&first.request_headers, "host"),
            Some("api.example.com")
        );

        let second = &log.entries[1];
        assert_eq!(second.method, "POST");
        assert_eq!(second.status, Some(401), "HTTP/2 的 :status 伪头");
        assert_eq!(second.path, "/v1/login");
    }
}
