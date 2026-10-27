//! JSON 语法分词：只回答「哪一段文本是什么」，颜色由视图按主题决定。
//!
//! 抓包正文的 Pretty 视图需要语法高亮。这里手写一个够用的 JSON 扫描器，而不是打开
//! gpui-component 的 `tree-sitter` 特性：那条路要给依赖树加上 tree-sitter 运行时和
//! 一次 C 编译，而本应用只需要高亮 JSON 一种语言。纯函数、不碰 GPUI，所以残缺输入
//! （正文预览被截断）也在这里直接测掉。

use std::ops::Range;

/// 一类 JSON 文本片段。视图把它映射到主题色，Tree 与 Pretty 因此共用一套配色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// 对象成员名：字符串后面紧跟 `:`。
    Key,
    String,
    Number,
    Bool,
    Null,
}

const LITERALS: &[(&str, Token)] = &[
    ("true", Token::Bool),
    ("false", Token::Bool),
    ("null", Token::Null),
];

/// 返回一段 JSON 文本里所有可着色的片段，按出现顺序排列。
///
/// 输入允许残缺：扫到读不懂的地方就停下，已识别的部分照常返回。返回的每个区间都
/// 落在字符边界上，可以直接交给 `StyledText::with_highlights`。
pub fn json(source: &str) -> Vec<(Range<usize>, Token)> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                match string_end(bytes, index) {
                    Some(end) => {
                        let token = if follows_colon(bytes, end + 1) {
                            Token::Key
                        } else {
                            Token::String
                        };
                        tokens.push((index..end + 1, token));
                        index = end + 1;
                    }
                    // 字符串没有闭合（正文被截断）：把剩下的都当成字符串染掉。
                    None => {
                        tokens.push((index..bytes.len(), Token::String));
                        break;
                    }
                }
            }
            b'-' | b'0'..=b'9' => {
                let mut end = index;
                while end < bytes.len()
                    && matches!(bytes[end], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    end += 1;
                }
                if end > index {
                    tokens.push((index..end, Token::Number));
                }
                index = end;
            }
            _ => {
                match LITERALS
                    .iter()
                    .find(|(word, _)| bytes[index..].starts_with(word.as_bytes()))
                {
                    Some((word, token)) => {
                        let end = index + word.len();
                        tokens.push((index..end, *token));
                        index = end;
                    }
                    None => index += 1,
                }
            }
        }
    }
    tokens
}

/// 从开头的 `"` 找到收尾的 `"`，跳过 `\"` 这类转义；找不到返回 `None`。
fn string_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut index = open + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some(index),
            _ => index += 1,
        }
    }
    None
}

/// 跳过空白后是不是 `:`——是就说明这个字符串是成员名而不是值。
fn follows_colon(bytes: &[u8], mut index: usize) -> bool {
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_whitespace())
    {
        index += 1;
    }
    bytes.get(index) == Some(&b':')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_values_are_told_apart() {
        let source = r#"{"a": 1, "b": "text"}"#;
        assert_eq!(
            json(source),
            vec![
                (1..4, Token::Key),
                (6..7, Token::Number),
                (9..12, Token::Key),
                (14..20, Token::String),
            ]
        );
    }

    #[test]
    fn escaped_quote_does_not_end_the_string() {
        let source = r#"{"say \"hi\"": 2}"#;
        assert_eq!(
            json(source),
            vec![(1..13, Token::Key), (15..16, Token::Number)]
        );
    }

    #[test]
    fn numbers_keep_their_sign_and_exponent() {
        let source = "[1e-5, -2.5, 3]";
        assert_eq!(
            json(source),
            vec![
                (1..5, Token::Number),
                (7..11, Token::Number),
                (13..14, Token::Number),
            ]
        );
    }

    #[test]
    fn literals_read_as_bool_and_null() {
        let source = r#"{"ok":true,"x":null}"#;
        assert_eq!(
            json(source),
            vec![
                (1..5, Token::Key),
                (6..10, Token::Bool),
                (11..14, Token::Key),
                (15..19, Token::Null),
            ]
        );
    }

    #[test]
    fn a_truncated_preview_still_highlights_what_arrived() {
        let source = "{\"a\": 1, \"b\": \"cut off";
        let tokens = json(source);
        assert_eq!(tokens.len(), 4);
        assert_eq!(tokens[3], (14..source.len(), Token::String));
    }

    /// 区间落在字符中间会让 `StyledText` 的调试断言炸掉，这里逐条检查边界。
    #[test]
    fn every_range_sits_on_a_char_boundary() {
        let source = "{\"键\":\"中文值\",\"n\":1.5,\"列表\":[true,null]}";
        for (range, _) in json(source) {
            assert!(
                source.is_char_boundary(range.start),
                "start {}",
                range.start
            );
            assert!(source.is_char_boundary(range.end), "end {}", range.end);
        }
    }
}
