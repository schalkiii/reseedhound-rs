//! sh-engine：流水线/网络层（M2 实现，当前占位）

pub mod cookiecloud;
pub mod downloader;
pub mod error;
pub mod feishu;
pub mod hdarea;
pub mod jackett;
pub mod savept;
pub mod scheduler;
pub mod seeder;
pub mod site_client;
pub mod weekly_report;

pub use cookiecloud::CookieCloudClient;
pub use downloader::{DownloaderClient, DownloaderConn, DownloaderKind};
pub use hdarea::{HdareaMatch, HdareaMatcher};
pub use savept::{DeadMatch, DeadSite};
pub use site_client::{DeadTorrentCache, DownloadReason, SiteClient};
