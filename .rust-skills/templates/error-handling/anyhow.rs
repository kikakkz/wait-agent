//! Error handling template for applications using anyhow
//!
//! Add to Cargo.toml:
//! ```toml
//! [dependencies]
//! anyhow = "1"
//! ```

use anyhow::{Context, Result, bail, ensure};

// =====================================================
// Main Application Pattern
// =====================================================

fn main() -> Result<()> {
    // Setup (logging, etc.)
    init_logging()?;

    // Run main logic
    if let Err(e) = run() {
        // Log full error chain
        eprintln!("Error: {:#}", e);

        // For debugging, show backtrace
        // RUST_BACKTRACE=1 cargo run
        std::process::exit(1);
    }

    Ok(())
}

fn init_logging() -> Result<()> {
    // Logging setup...
    Ok(())
}

fn run() -> Result<()> {
    let config = load_config("config.toml")
        .context("failed to load configuration")?;

    let db = connect_database(&config.db_url)
        .context("failed to connect to database")?;

    process_data(&db)
        .context("failed to process data")?;

    Ok(())
}

// =====================================================
// Error Context Pattern
// =====================================================

fn load_config(path: &str) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading config file: {}", path))?;

    let config: Config = parse_config(&content)
        .context("parsing config")?;

    validate_config(&config)?;

    Ok(config)
}

fn validate_config(config: &Config) -> Result<()> {
    // ensure! macro for preconditions
    ensure!(config.port > 0, "port must be positive, got {}", config.port);
    ensure!(!config.db_url.is_empty(), "database URL cannot be empty");

    // bail! for explicit error
    if config.timeout_secs == 0 {
        bail!("timeout cannot be zero");
    }

    Ok(())
}

// =====================================================
// Error Propagation Pattern
// =====================================================

fn process_data(db: &Database) -> Result<()> {
    let users = db.get_users()
        .context("fetching users")?;

    for user in users {
        process_user(&user)
            .with_context(|| format!("processing user: {}", user.id))?;
    }

    Ok(())
}

fn process_user(user: &User) -> Result<()> {
    let data = fetch_user_data(user.id)
        .with_context(|| format!("fetching data for user {}", user.id))?;

    transform_data(&data)
        .context("transforming data")?;

    save_result(&data)
        .context("saving result")?;

    Ok(())
}

// =====================================================
// Combining with Option
// =====================================================

fn find_user_email(id: u64) -> Result<String> {
    let user = get_user_by_id(id)
        .context("looking up user")?
        .ok_or_else(|| anyhow::anyhow!("user {} not found", id))?;

    user.email
        .ok_or_else(|| anyhow::anyhow!("user {} has no email", id))
}

// =====================================================
// Downcast Pattern (when needed)
// =====================================================

fn handle_error(err: anyhow::Error) {
    // Try to downcast to specific error type
    if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
        match io_err.kind() {
            std::io::ErrorKind::NotFound => {
                println!("File not found, using defaults");
            }
            std::io::ErrorKind::PermissionDenied => {
                println!("Permission denied");
            }
            _ => {
                println!("IO error: {}", io_err);
            }
        }
    } else {
        println!("Error: {:#}", err);
    }
}

// =====================================================
// Placeholder Types
// =====================================================

struct Config {
    port: u16,
    db_url: String,
    timeout_secs: u64,
}

struct Database;
struct User {
    id: u64,
    email: Option<String>,
}
struct Data;

fn parse_config(_content: &str) -> Result<Config> {
    Ok(Config {
        port: 8080,
        db_url: "postgres://localhost/db".to_string(),
        timeout_secs: 30,
    })
}

fn connect_database(_url: &str) -> Result<Database> {
    Ok(Database)
}

impl Database {
    fn get_users(&self) -> Result<Vec<User>> {
        Ok(vec![])
    }
}

fn get_user_by_id(_id: u64) -> Result<Option<User>> {
    Ok(Some(User { id: 1, email: Some("test@example.com".to_string()) }))
}

fn fetch_user_data(_id: u64) -> Result<Data> {
    Ok(Data)
}

fn transform_data(_data: &Data) -> Result<()> {
    Ok(())
}

fn save_result(_data: &Data) -> Result<()> {
    Ok(())
}
