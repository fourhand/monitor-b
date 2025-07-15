use mdns_sd::{ServiceDaemon, ServiceInfo};
use std::collections::HashMap;

pub async fn broadcast_mdns() {
    // Create a daemon
    let mdns = ServiceDaemon::new().expect("Failed to create mDNS daemon");

    // Service details
    let service_type = "_http._tcp.local.";
    let instance_name = "m32-server";
    let host_name = "m32-server.local.";
    let port = 8000;
    let properties = HashMap::new();

    // Create service info
    let my_service = ServiceInfo::new(
        service_type,
        instance_name,
        host_name,
        "0.0.0.0",
        port,
        properties, // pass as value, not reference
    ).expect("Failed to create ServiceInfo");

    // Register the service
    mdns.register(my_service).expect("Failed to register mDNS service");

    // Keep the daemon alive
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
} 