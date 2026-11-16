fn main() {
    println!("B-Web {}", bweb_core::VERSION);
    println!("Stage: {:?}", bweb_core::EngineStage::Bootstrap);
    println!("Privacy engine: {}", bweb_shield::FILTER_ENGINE);
    println!("JavaScript runtime: {:?}", bweb_js::RuntimeStatus::NotImplemented);
}
