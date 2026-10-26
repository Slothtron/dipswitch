//! dipswitch：个人本机控制台。字体管理与用户级环境变量管理共用一个窗口外壳。
//!
//! 拆成库 + 二进制两个目标是为了让测试能直接引用真实视图与真实模块，
//! 而不是复制一份近似的测试视图。
//!
//! 模块分工：
//!
//! - [`shell`]：窗口外壳（标题栏、左侧导航、状态栏）与模块路由；
//! - [`fonts_view`] / [`environment_view`]：两个模块的界面；
//! - [`env_model`] / [`env_service`] / [`system_env`]：环境变量分组的分层解析、
//!   落地编排与平台适配，三层各自可测；
//! - [`store`]：SQLite 配置库；[`settings`]：偏好与旧 JSON 的一次性导入。

pub mod env_model;
pub mod env_service;
pub mod environment_view;
pub mod fonts;
pub mod fonts_view;
pub mod settings;
pub mod shell;
pub mod size_slider;
pub mod store;
pub mod system_env;
