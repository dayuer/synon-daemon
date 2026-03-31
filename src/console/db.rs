use deadpool_sqlite::{Config, Pool, Runtime};
use std::path::PathBuf;

pub async fn init_db_pool() -> Result<Pool, String> {
    // 默认尝试猜测上级目录中的 synonclaw/data/registry/nodes.db
    let db_path = std::env::var("SYNONCLAW_DB_PATH").unwrap_or_else(|_| {
        let current_dir = std::env::current_dir().unwrap_or_default();
        let mut path = current_dir.parent().unwrap_or(&current_dir).to_path_buf();
        path.push("synonclaw");
        path.push("data");
        path.push("registry");
        path.push("nodes.db");
        path.to_string_lossy().to_string()
    });

    let path = PathBuf::from(&db_path);
    if !path.exists() {
        tracing::warn!("警告：指定的数据库文件不存在 at {}（将自动创建新库，但可能丢失 Console 数据）", db_path);
    } else {
        tracing::info!("使用 Console 数据库: {}", db_path);
    }

    let cfg = Config::new(db_path);
    let pool = cfg.create_pool(Runtime::Tokio1)
        .map_err(|e| format!("创建 Pool 失败: {}", e))?;

    // 初始化时强制启用 WAL 模式以支持与 Next.js 并发读写
    let conn = pool.get().await.map_err(|e| format!("获取连接失败: {}", e))?;
    
    conn.interact(|conn| {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA cache_size=-64000;
             PRAGMA busy_timeout=5000;"
        )
    })
    .await
    .map_err(|e| format!("与底库交互失败: {}", e))?
    .map_err(|e| format!("设置 PRAGMA 失败: {}", e))?;

    Ok(pool)
}
