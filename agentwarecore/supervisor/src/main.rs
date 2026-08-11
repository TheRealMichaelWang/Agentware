fn main() {
    println!("\n====================================");
    println!(" Hello from your custom Agent OS! ");
    println!("====================================\n");
    
    // PID 1 must never exit, otherwise the kernel will panic!
    loop {}
}
