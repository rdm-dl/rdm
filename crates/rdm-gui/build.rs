fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/rdm.ico");
        res.set("ProductName", "RDM");
        res.set("FileDescription", "RDM Download Manager");
        res.set("CompanyName", "RDM");
        res.set("LegalCopyright", "MIT License");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed icon: {e}");
        }
    }
    println!("cargo:rerun-if-changed=../../assets/rdm.ico");
}
