use libsystemd::id128;

fn main() {
    let app_id = id128::Id128::parse_str("47c2a52ec65947ae88b2b08d14ec126b")
        .expect("Failed to parse ID string");

    println!("get_machine:              {:?}", id128::get_machine());
    println!(
        "get_machine_app_specific: {:?}",
        id128::get_machine_app_specific(&app_id)
    );

    println!("get_boot:                 {:?}", id128::get_boot());
    println!(
        "get_boot_app_specific:    {:?}",
        id128::get_boot_app_specific(&app_id)
    );
}
