fn main() {
    #[cfg(target_os = "windows")]
    {
        embed_manifest::embed_manifest_file("pincer-uia.manifest")
            .expect("failed to embed pincer-uia.manifest");
        println!("cargo:rerun-if-changed=pincer-uia.manifest");
    }
}
