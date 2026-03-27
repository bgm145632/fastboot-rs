use anyhow::Result;
use clap::Parser;

use crate::boot_patch::{BootPatchArgs, BootRestoreArgs, GetKmiArgs};
use crate::defs;

/// KernelSU cli for non-android
#[derive(Parser, Debug)]
#[command(author, version = defs::VERSION_NAME, about = "KernelSU PATCH \n 编译者：雨纷飞 ", long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Patch boot or init_boot images to apply KernelSU
    BootPatch(BootPatchArgs),

    /// Restore boot or init_boot images patched by KernelSU
    BootRestore(BootRestoreArgs),
    // /// Get apk size and hash
    // GetSign {
    //     /// apk path
    //     apk: String,
    // },
    GetKmi(GetKmiArgs),
    // /// show supported kmi versions
    // SupportedKmis,
}

pub fn run() -> Result<()> {
    env_logger::init();

    let cli = Args::parse();

    log::info!("command: {:?}", cli.command);

    let result = match cli.command {
        Commands::BootPatch(boot_patch) => crate::boot_patch::patch(boot_patch),

        Commands::BootRestore(boot_restore) => crate::boot_patch::restore(boot_restore),
        // // Commands::GetSign { apk } => { ... }
        // // Commands::SupportedKmis => { ... }
        Commands::GetKmi(get_kmi_args) => crate::boot_patch::get_kmi(get_kmi_args),
    };

    if let Err(e) = &result {
        log::error!("Error: {e:?}");
    }
    result
}