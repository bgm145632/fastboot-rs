fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "android")]
    {
        ksud::cli::run()
    }
    #[cfg(not(target_os = "android"))]
    {
        ksud::cli_non_android::run()
    }
}
