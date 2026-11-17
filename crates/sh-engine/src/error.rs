//! 引擎层错误类型

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("下载器连接失败")]
    DownloaderConnect,
    #[error("下载器配置缺少 host：请检查 config.yaml 的 downloader 块（双下载器模式需分别填写 source.host 与 destination.host）")]
    DownloaderMissingHost,
    #[error("不支持的下载器类型: {0}")]
    UnsupportedDownloaderType(String),
    #[error("种子目录未配置：请在 config.yaml 中填写 downloader.torrent_dir")]
    MissingTorrentDir,
    #[error("HTTP 客户端构建失败: {0}")]
    HttpClientBuild(#[from] reqwest::Error),
}
