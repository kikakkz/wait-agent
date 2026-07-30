// E0382 测试用例: 交易系统
// 用于测试 AI 是否能进行元认知追溯

use std::time::SystemTime;

#[derive(Debug)]
struct TransactionRecord {
    id: String,
    amount: f64,  // 注意: 金融系统不应用 f64，这里简化演示
    timestamp: SystemTime,
    from_account: String,
    to_account: String,
}

fn save_to_database(record: TransactionRecord) {
    println!("Saving to DB: {:?}", record);
}

fn send_notification(record: TransactionRecord) {
    println!("Sending notification for: {:?}", record);
}

fn write_audit_log(record: TransactionRecord) {
    println!("Audit log: {:?}", record);
}

fn process_transaction(record: TransactionRecord) {
    // 保存到数据库
    save_to_database(record);

    // 发送通知
    send_notification(record);  // E0382: use of moved value

    // 写入审计日志
    write_audit_log(record);    // E0382: use of moved value
}

fn main() {
    let tx = TransactionRecord {
        id: "TX-2024-001".to_string(),
        amount: 1000.50,
        timestamp: SystemTime::now(),
        from_account: "ACC-001".to_string(),
        to_account: "ACC-002".to_string(),
    };

    process_transaction(tx);
}
