# Meta-Cognition 测试套件

> 设计用于区分「普通 AI 回答」vs「元认知追溯回答」的测试用例

## 设计原则

好的测试用例应该：
1. **普通回答能"解决"问题** - 语法正确，能编译
2. **但普通回答有隐藏问题** - 违反领域约束或设计原则
3. **只有理解领域后才能给出正确建议**

---

## 测试用例 1: 金融精度陷阱

### 问题

```
我在写一个支付系统，计算手续费时发现金额不对：

fn calculate_fee(amount: f64, rate: f64) -> f64 {
    amount * rate
}

fn main() {
    let amount = 0.1 + 0.2;
    let fee = calculate_fee(amount, 0.03);
    println!("Fee: {}", fee);  // 输出 0.009000000000000001 而不是 0.009
}

怎么修复这个精度问题？
```

### 回答对比

| 层次 | 回答 | 问题 |
|------|------|------|
| ⭐ 表面 | `format!("{:.2}", fee)` 格式化输出 | 只是隐藏问题，内部仍错误 |
| ⭐⭐ 语法 | 用 `f64::round()` 四舍五入 | 累积误差仍会出现 |
| ⭐⭐⭐ 机制 | 用整数分为单位计算 | 可行但不专业 |
| ⭐⭐⭐⭐⭐ 元认知 | **金融系统禁止用浮点数，必须用 rust_decimal** | 理解领域约束 |

### 元认知追溯

```
Layer 1: 浮点精度问题 → IEEE 754 表示限制
    ↑
Layer 3: 金融领域约束 → 精度是法规要求，不是技术选择
    ↓
Layer 2: 使用 rust_decimal::Decimal 类型
```

### 正确答案

```rust
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn calculate_fee(amount: Decimal, rate: Decimal) -> Decimal {
    amount * rate
}

fn main() {
    let amount = dec!(0.1) + dec!(0.2);  // 精确的 0.3
    let fee = calculate_fee(amount, dec!(0.03));
    println!("Fee: {}", fee);  // 精确的 0.009
}
```

---

## 测试用例 2: 并发共享陷阱

### 问题

```
我的 Web API 需要共享配置，但编译器报错：

use std::rc::Rc;

struct AppConfig {
    db_url: String,
    api_key: String,
}

async fn handle_request(config: Rc<AppConfig>) {
    // 使用配置...
}

#[tokio::main]
async fn main() {
    let config = Rc::new(AppConfig {
        db_url: "postgres://...".into(),
        api_key: "secret".into(),
    });

    tokio::spawn(handle_request(config.clone()));  // 编译错误
}

错误: `Rc<AppConfig>` cannot be sent between threads safely
怎么解决？
```

### 回答对比

| 层次 | 回答 | 问题 |
|------|------|------|
| ⭐ 表面 | 把 `Rc` 改成 `Arc` | 正确但不完整 |
| ⭐⭐ 语法 | `Arc<AppConfig>` + 解释 Send trait | 技术正确 |
| ⭐⭐⭐ 机制 | 解释 Rc vs Arc 的区别 | 还是技术层面 |
| ⭐⭐⭐⭐⭐ 元认知 | **Web 服务是多线程的，配置应该用 `&'static` 或 `OnceLock`，Arc 只是次优解** | 理解 Web 领域 |

### 元认知追溯

```
Layer 1: Rc 不是 Send → 不能跨线程
    ↑
Layer 3: Web 服务领域约束 → 高并发、多线程、配置不可变
    ↓
Layer 2: 配置模式选择：
    - OnceLock<AppConfig> (推荐，零运行时开销)
    - lazy_static! (经典方案)
    - Arc<AppConfig> (可行但有开销)
```

### 正确答案

```rust
use std::sync::OnceLock;

static CONFIG: OnceLock<AppConfig> = OnceLock::new();

fn get_config() -> &'static AppConfig {
    CONFIG.get_or_init(|| AppConfig {
        db_url: std::env::var("DATABASE_URL").unwrap(),
        api_key: std::env::var("API_KEY").unwrap(),
    })
}

async fn handle_request() {
    let config = get_config();  // 零开销，无 clone
    // 使用配置...
}
```

---

## 测试用例 3: 错误处理陷阱

### 问题

```
我的 CLI 工具处理文件时经常 panic：

fn process_file(path: &str) -> String {
    let content = std::fs::read_to_string(path).unwrap();
    let config: Config = serde_json::from_str(&content).unwrap();
    config.name.to_uppercase()
}

用户说文件不存在时程序就崩溃了，怎么改成更友好的错误提示？
```

### 回答对比

| 层次 | 回答 | 问题 |
|------|------|------|
| ⭐ 表面 | 用 `expect("文件不存在")` | 还是会 panic |
| ⭐⭐ 语法 | 返回 `Result<String, Box<dyn Error>>` | 技术正确但不专业 |
| ⭐⭐⭐ 机制 | 用 `anyhow` 或 `thiserror` | 更好但没考虑 CLI 场景 |
| ⭐⭐⭐⭐⭐ 元认知 | **CLI 工具应该：1) 用 anyhow 简化错误 2) 在 main 统一处理 3) 返回正确的 exit code 4) 用 miette 美化输出** | 理解 CLI 领域 |

### 元认知追溯

```
Layer 1: unwrap() panic → 需要错误传播
    ↑
Layer 3: CLI 领域约束 → 用户体验、exit code、可脚本化
    ↓
Layer 2: CLI 错误处理模式：
    - anyhow::Result 简化错误链
    - main() -> Result<(), anyhow::Error>
    - 或用 miette 美化错误显示
```

### 正确答案

```rust
use anyhow::{Context, Result};

fn process_file(path: &str) -> Result<String> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("无法读取文件: {}", path))?;

    let config: Config = serde_json::from_str(&content)
        .with_context(|| format!("无法解析配置文件: {}", path))?;

    Ok(config.name.to_uppercase())
}

fn main() -> Result<()> {
    let result = process_file("config.json")?;
    println!("{}", result);
    Ok(())
}

// 或者用 miette 获得更美观的错误输出
```

---

## 测试用例 4: 异步阻塞陷阱

### 问题

```
我的 API 处理大文件时很慢，其他请求都被阻塞了：

async fn upload_handler(data: Vec<u8>) -> Result<String, Error> {
    // 压缩文件
    let compressed = compress(&data);  // 这是 CPU 密集操作

    // 计算哈希
    let hash = sha256(&compressed);    // 这也是 CPU 密集操作

    // 保存到存储
    storage.save(&hash, &compressed).await?;

    Ok(hash)
}

为什么会阻塞其他请求？怎么解决？
```

### 回答对比

| 层次 | 回答 | 问题 |
|------|------|------|
| ⭐ 表面 | 加 `.await` | 根本没理解问题 |
| ⭐⭐ 语法 | 用 `tokio::spawn` 并行处理 | 还是会阻塞线程 |
| ⭐⭐⭐ 机制 | 用 `spawn_blocking` | 正确但不完整 |
| ⭐⭐⭐⭐⭐ 元认知 | **理解 tokio 运行时模型：CPU 密集任务必须用 spawn_blocking，或者用 rayon 线程池，并考虑背压控制** | 理解异步领域 |

### 元认知追溯

```
Layer 1: async 函数中有同步阻塞 → 阻塞了 tokio worker
    ↑
Layer 3: Web 服务领域约束 → 高并发、低延迟、不能阻塞事件循环
    ↓
Layer 2: 异步架构模式：
    - CPU 密集 → spawn_blocking 或 rayon
    - I/O 密集 → 保持 async
    - 混合 → 分离关注点
```

### 正确答案

```rust
async fn upload_handler(data: Vec<u8>) -> Result<String, Error> {
    // CPU 密集操作移到阻塞线程池
    let (compressed, hash) = tokio::task::spawn_blocking(move || {
        let compressed = compress(&data);
        let hash = sha256(&compressed);
        (compressed, hash)
    }).await?;

    // I/O 操作保持异步
    storage.save(&hash, &compressed).await?;

    Ok(hash)
}
```

---

## 测试方法

### 1. 给普通 Claude

直接粘贴问题代码，看回答停在哪个层次。

### 2. 给带 rust-skills 的 Claude

同样的问题，应该看到：
- 明确的层次追溯 (Layer 1 → 3 → 2)
- 领域约束的识别
- 不仅仅是"能编译"的解决方案

### 3. 评分标准

| 维度 | 普通回答 | 元认知回答 |
|------|----------|------------|
| 修复错误 | ✅ | ✅ |
| 解释机制 | 可能 | ✅ |
| 识别领域约束 | ❌ | ✅ |
| 给出最佳实践 | ❌ | ✅ |
| 解释为什么是最佳 | ❌ | ✅ |
| 提到相关 crate | 可能 | ✅ (带版本) |

---

## 快速测试模板

复制以下内容直接测试：

```
我在开发一个支付系统，计算手续费时发现精度有问题：

let amount = 0.1 + 0.2;
let fee = amount * 0.03;
println!("Fee: {}", fee);  // 输出 0.009000000000000001

怎么修复？
```

**期望差异**:
- 普通: "用 round() 或格式化输出"
- 元认知: "金融系统禁止用 f64，必须用 rust_decimal，这是监管要求"
