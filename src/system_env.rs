//! 用户级环境变量的平台适配层。
//!
//! 语义契约在所有平台上保持一致：**读取用户环境变量、写入、删除**；差别只在
//! 写入落点：
//!
//! - Windows：读写 `HKCU\Environment`（真正的用户级持久化），写完同步当前进程
//!   环境并广播 `WM_SETTINGCHANGE`，让 shell 与新进程拿到新值。
//! - 其他平台：只能作用于当前进程环境，进程退出即失效；界面必须如实说明
//!   （见 [`is_persistent`] 与 [`scope_label`]）。
//!
//! 值类型保留注册表语义：`REG_SZ`、`REG_EXPAND_SZ`、`REG_MULTI_SZ`。多字符串
//! 的内部表示用 `\0` 连接各项，因此原始边界可以无损写回；展示时才用 `; ` 分隔，
//! 并且禁止直接编辑（分隔符本身就是数据）。

/// 用户环境变量的存储类型，对应注册表的三种字符串值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    /// `REG_SZ`：普通字符串。
    Plain,
    /// `REG_EXPAND_SZ`：含 `%VAR%` 之类需要展开的字符串。
    Expandable,
    /// `REG_MULTI_SZ`：多字符串，只读展示。
    MultiSz,
}

impl ValueKind {
    /// 注册表值类型名，用于说明这一行的存储语义。
    pub fn label(self) -> &'static str {
        match self {
            Self::Plain => "REG_SZ",
            Self::Expandable => "REG_EXPAND_SZ",
            Self::MultiSz => "REG_MULTI_SZ",
        }
    }

    /// 数据库里存的是 [`ValueKind::label`]，读取时还原。
    pub fn parse(label: &str) -> Self {
        match label {
            "REG_EXPAND_SZ" => Self::Expandable,
            "REG_MULTI_SZ" => Self::MultiSz,
            _ => Self::Plain,
        }
    }
}

/// 一条用户环境变量。`raw` 对多字符串是 `\0` 连接的原始各项。
#[derive(Debug, Clone)]
pub struct SystemVariable {
    key: String,
    raw: String,
    kind: ValueKind,
}

impl SystemVariable {
    pub fn new(key: impl Into<String>, raw: impl Into<String>, kind: ValueKind) -> Self {
        Self {
            key: key.into(),
            raw: raw.into(),
            kind,
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// 内部表示；多字符串保留 `\0` 边界。
    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn kind(&self) -> ValueKind {
        self.kind
    }

    /// 面向展示与编辑的值；多字符串按 `; ` 分隔。
    pub fn display_value(&self) -> String {
        match self.kind {
            ValueKind::MultiSz => self
                .raw
                .split('\0')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("; "),
            ValueKind::Plain | ValueKind::Expandable => self.raw.clone(),
        }
    }

    /// 多字符串无法安全往返编辑，因此只读。
    pub fn is_editable(&self) -> bool {
        self.kind != ValueKind::MultiSz
    }
}

/// 写入落点是否跨进程持久。
pub fn is_persistent() -> bool {
    cfg!(windows)
}

/// 写入落点的名称，用于在界面上如实说明作用域。
pub fn scope_label() -> &'static str {
    if cfg!(windows) {
        r"HKCU\Environment"
    } else {
        "当前进程环境"
    }
}

/// 读取全部用户环境变量，按 key 升序。
pub fn read_variables() -> Result<Vec<SystemVariable>, String> {
    #[cfg(windows)]
    {
        win::read_variables()
    }
    #[cfg(not(windows))]
    {
        let mut variables = std::env::vars()
            .map(|(key, value)| SystemVariable::new(key, value, ValueKind::Plain))
            .collect::<Vec<_>>();
        variables.sort_by(|a, b| a.key().cmp(b.key()));
        Ok(variables)
    }
}

/// 写入（新增或覆盖）一条变量，并让当前进程立即看到新值。
pub fn write_variable(variable: &SystemVariable) -> Result<(), String> {
    #[cfg(windows)]
    {
        win::write_variable(variable)?;
        win::sync_process_environment(variable.key(), Some(variable.raw()));
        win::broadcast_environment_change();
    }
    #[cfg(not(windows))]
    {
        // SAFETY: 工作台的环境变量写入都在主线程的 UI 回调里串行发生。
        unsafe { std::env::set_var(variable.key(), variable.raw()) };
    }
    Ok(())
}

/// 删除一条变量；key 本来不存在视为已完成。
pub fn remove_variable(key: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        win::remove_variable(key)?;
        win::sync_process_environment(key, None);
        win::broadcast_environment_change();
    }
    #[cfg(not(windows))]
    {
        // SAFETY: 同 `write_variable`。
        unsafe { std::env::remove_var(key) };
    }
    Ok(())
}

#[cfg(windows)]
mod win {
    use std::borrow::Cow;

    use winreg::RegKey;
    use winreg::enums::{
        HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_EXPAND_SZ, REG_MULTI_SZ, REG_SZ,
    };
    use winreg::reg_value::RegValue;

    use super::{SystemVariable, ValueKind};

    const SUBKEY: &str = "Environment";

    fn environment_key(create: bool) -> Result<RegKey, String> {
        let root = RegKey::predef(HKEY_CURRENT_USER);
        if create {
            root.create_subkey_with_flags(SUBKEY, KEY_READ | KEY_WRITE)
                .map(|(key, _)| key)
                .map_err(|error| format!("无法打开 HKCU\\{SUBKEY}：{error}"))
        } else {
            root.open_subkey_with_flags(SUBKEY, KEY_READ)
                .map_err(|error| format!("无法打开 HKCU\\{SUBKEY}：{error}"))
        }
    }

    fn units_to_bytes(units: &[u16]) -> Vec<u8> {
        units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
    }

    fn text_units(text: &str) -> Vec<u16> {
        let mut units = text.encode_utf16().collect::<Vec<_>>();
        units.push(0);
        units
    }

    fn decode_text(bytes: &[u8]) -> String {
        let units = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect::<Vec<_>>();
        String::from_utf16_lossy(units.as_slice())
            .trim_end_matches('\0')
            .to_string()
    }

    /// 多字符串解码为 `\0` 连接的原始各项，边界可无损还原。
    fn decode_multi(bytes: &[u8]) -> String {
        let units = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect::<Vec<_>>();
        String::from_utf16_lossy(units.as_slice())
            .trim_end_matches('\0')
            .to_string()
    }

    fn encode_multi(raw: &str) -> Vec<u8> {
        let mut units = Vec::new();
        for part in raw.split('\0').filter(|part| !part.is_empty()) {
            units.extend(part.encode_utf16());
            units.push(0);
        }
        units.push(0);
        units_to_bytes(&units)
    }

    fn to_reg_value(variable: &SystemVariable) -> RegValue<'static> {
        let (bytes, vtype) = match variable.kind() {
            ValueKind::Plain => (units_to_bytes(&text_units(variable.raw())), REG_SZ),
            ValueKind::Expandable => (units_to_bytes(&text_units(variable.raw())), REG_EXPAND_SZ),
            ValueKind::MultiSz => (encode_multi(variable.raw()), REG_MULTI_SZ),
        };
        RegValue {
            bytes: Cow::Owned(bytes),
            vtype,
        }
    }

    pub fn read_variables() -> Result<Vec<SystemVariable>, String> {
        let key = environment_key(false)?;
        let mut variables = Vec::new();
        for (name, value) in key.enum_values().filter_map(Result::ok) {
            if name.is_empty() {
                continue;
            }
            let variable = match value.vtype {
                REG_SZ => SystemVariable::new(name, decode_text(&value.bytes), ValueKind::Plain),
                REG_EXPAND_SZ => {
                    SystemVariable::new(name, decode_text(&value.bytes), ValueKind::Expandable)
                }
                REG_MULTI_SZ => {
                    SystemVariable::new(name, decode_multi(&value.bytes), ValueKind::MultiSz)
                }
                // DWORD/QWORD/binary 不属于本模块的管理范围，列出来只会误导
                // 用户去改坏它，因此不展示。
                _ => continue,
            };
            variables.push(variable);
        }
        variables.sort_by(|a, b| a.key().cmp(b.key()));
        Ok(variables)
    }

    pub fn write_variable(variable: &SystemVariable) -> Result<(), String> {
        let key = environment_key(true)?;
        key.set_raw_value(variable.key(), &to_reg_value(variable))
            .map_err(|error| format!("写入 {} 失败：{error}", variable.key()))
    }

    pub fn remove_variable(key_name: &str) -> Result<(), String> {
        let key = environment_key(true)?;
        match key.delete_value(key_name) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("删除 {key_name} 失败：{error}")),
        }
    }

    /// 让本进程也带上刚写入的值，界面上的「生效值」才与现实一致。
    pub fn sync_process_environment(key: &str, value: Option<&str>) {
        // SAFETY: 环境变量写入都在主线程串行执行，且只发生在 UI 回调中。
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }

    /// 广播 `WM_SETTINGCHANGE`，让 shell 与新进程重新读取用户环境。
    pub fn broadcast_environment_change() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
        };

        let parameter = "Environment"
            .encode_utf16()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut result = 0usize;
        // SAFETY: `parameter` 在调用期间保持存活；HWND_BROADCAST 是系统约定。
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                parameter.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                2000,
                &mut result,
            )
        };
    }

    /// 编码/解码的测试入口：注册表字节层是这套适配里最容易写错的一段，
    /// 用纯内存断言覆盖，不去碰真实注册表。
    #[cfg(test)]
    pub(super) fn encode(variable: &SystemVariable) -> (Vec<u8>, winreg::enums::RegType) {
        let value = to_reg_value(variable);
        (value.bytes.to_vec(), value.vtype)
    }

    #[cfg(test)]
    pub(super) fn decode(bytes: &[u8], vtype: winreg::enums::RegType) -> Option<SystemVariable> {
        use winreg::enums::RegType;
        let (kind, raw) = match vtype {
            RegType::REG_SZ => (ValueKind::Plain, decode_text(bytes)),
            RegType::REG_EXPAND_SZ => (ValueKind::Expandable, decode_text(bytes)),
            RegType::REG_MULTI_SZ => (ValueKind::MultiSz, decode_multi(bytes)),
            _ => return None,
        };
        Some(SystemVariable::new("PROBE", raw, kind))
    }
}

#[cfg(all(test, windows))]
mod codec_tests {
    use winreg::enums::RegType;

    use super::win::{decode, encode};
    use super::{SystemVariable, ValueKind};

    /// 真实注册表往返：写一个只属于本测试的 key，读回校验后删除。
    ///
    /// 默认不跑（`#[ignore]`），因为它会真的动 `HKCU\Environment` 并广播
    /// `WM_SETTINGCHANGE`。需要验证平台适配层时人工执行：
    /// `cargo test -- --ignored`。
    #[test]
    #[ignore = "会写入 HKCU\\Environment，需人工确认后再跑"]
    fn registry_round_trip_on_a_scratch_key() {
        const PROBE: &str = "DIPSWITCH_SELFTEST_PROBE";

        let variable = SystemVariable::new(
            PROBE,
            r"%LOCALAPPDATA%\dipswitch-probe",
            ValueKind::Expandable,
        );
        super::write_variable(&variable).expect("写入探针变量");

        let read_back = super::read_variables()
            .expect("读取用户环境")
            .into_iter()
            .find(|item| item.key() == PROBE)
            .expect("探针变量应能读回");
        assert_eq!(read_back.raw(), variable.raw());
        assert_eq!(read_back.kind(), ValueKind::Expandable);
        assert_eq!(
            std::env::var(PROBE).ok().as_deref(),
            Some(variable.raw()),
            "写入后当前进程也应看到新值"
        );

        super::remove_variable(PROBE).expect("删除探针变量");
        let still_there = super::read_variables()
            .expect("读取用户环境")
            .iter()
            .any(|item| item.key() == PROBE);
        assert!(!still_there, "探针变量应已清走");
    }

    #[test]
    fn plain_string_survives_the_registry_encoding() {
        let variable = SystemVariable::new("PROBE", r"C:\tools\bin", ValueKind::Plain);
        let (bytes, vtype) = encode(&variable);
        let back = decode(&bytes, vtype).expect("REG_SZ 应能解回");
        assert_eq!(back.kind(), ValueKind::Plain);
        assert_eq!(back.raw(), r"C:\tools\bin");
    }

    #[test]
    fn expandable_string_keeps_its_type_and_percent_signs() {
        let variable = SystemVariable::new(
            "PROBE",
            r"%USERPROFILE%\bin;%LOCALAPPDATA%\Programs",
            ValueKind::Expandable,
        );
        let (bytes, vtype) = encode(&variable);
        let back = decode(&bytes, vtype).expect("REG_EXPAND_SZ 应能解回");
        assert_eq!(back.kind(), ValueKind::Expandable);
        assert_eq!(back.raw(), r"%USERPROFILE%\bin;%LOCALAPPDATA%\Programs");
    }

    #[test]
    fn multi_string_keeps_every_item_boundary() {
        let raw = "first\0second with spaces\0third;with;semicolons";
        let variable = SystemVariable::new("PROBE", raw, ValueKind::MultiSz);
        let (bytes, vtype) = encode(&variable);
        let back = decode(&bytes, vtype).expect("REG_MULTI_SZ 应能解回");
        assert_eq!(back.kind(), ValueKind::MultiSz);
        assert_eq!(back.raw(), raw);
        // 展示时才用 `; ` 分隔；分隔符本身是数据，不参与还原。
        assert_eq!(
            back.display_value(),
            "first; second with spaces; third;with;semicolons"
        );
    }

    #[test]
    fn unsupported_registry_types_are_not_decoded_as_text() {
        // REG_DWORD（值 4）不该被当成字符串读出来。
        assert!(decode(&[4, 0, 0, 0], RegType::REG_DWORD).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multi_string_keeps_boundaries_for_round_trip() {
        let variable = SystemVariable::new("Path", "C:\\a\0C:\\b", ValueKind::MultiSz);
        assert_eq!(variable.display_value(), r"C:\a; C:\b");
        assert!(!variable.is_editable());
    }

    #[test]
    fn kinds_round_trip_through_their_labels() {
        for kind in [ValueKind::Plain, ValueKind::Expandable, ValueKind::MultiSz] {
            assert_eq!(ValueKind::parse(kind.label()), kind);
        }
    }

    #[test]
    fn plain_and_expandable_display_their_raw_value() {
        let variable = SystemVariable::new("PATH", r"%USERPROFILE%\bin", ValueKind::Expandable);
        assert_eq!(variable.display_value(), r"%USERPROFILE%\bin");
        assert!(variable.is_editable());
    }
}
