use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};
use gw_config::Config;
use gw_store::readonly::StoreReadonly;
use gw_store::schema::COLUMNS;
use gw_store::traits::chain_store::ChainStore;
use gw_types::packed;
use gw_types::prelude::{Entity, Unpack};
use indicatif::{ProgressBar, ProgressStyle};

pub struct ExportArgs {
    pub config: Config,
    pub output: PathBuf,
    pub from_block: Option<u64>,
    pub to_block: Option<u64>,
    pub show_progress: bool,
}

/// ExportBlock
///
/// Support export block from readonly database (don't need to exit node process)
pub struct ExportBlock {
    snap: StoreReadonly,
    output: PathBuf,
    from_block: u64,
    to_block: u64,
    progress_bar: Option<ProgressBar>,
}

impl ExportBlock {
    // Disable warning for bin
    #[allow(dead_code)]
    pub fn new_unchecked(
        snap: StoreReadonly,
        output: PathBuf,
        from_block: u64,
        to_block: u64,
    ) -> Self {
        ExportBlock {
            snap,
            output,
            from_block,
            to_block,
            progress_bar: None,
        }
    }

    pub fn create(args: ExportArgs) -> Result<Self> {
        let snap =
            StoreReadonly::open(&args.config.store.path, COLUMNS).context("open database")?;

        let db_last_valid_tip_block_number =
            snap.get_last_valid_tip_block()?.raw().number().unpack();

        let from_block = args.from_block.unwrap_or(0);
        let to_block = match args.to_block {
            Some(to) => {
                snap.get_block_hash_by_number(to)?
                    .ok_or_else(|| anyhow!("{} block not found", to))?;

                // TODO: support export bad block? (change `insert_bad_block` func to also include
                // deposit requests, deposit asset scripts and withdrawals). then add new arg
                // --skip-tip-bad-block-check. (also update file name).
                if to > db_last_valid_tip_block_number {
                    bail!(
                        "bad block found, start from block {}",
                        db_last_valid_tip_block_number + 1
                    );
                }

                to
            }
            None => db_last_valid_tip_block_number,
        };
        if from_block > to_block {
            bail!("from {} is bigger than to {}", from_block, to_block);
        }

        let progress_bar = if args.show_progress {
            let bar = ProgressBar::new(to_block.saturating_sub(from_block) + 1);
            bar.set_style(
                ProgressStyle::default_bar()
                    .template("[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}")
                    .progress_chars("##-"),
            );
            Some(bar)
        } else {
            None
        };

        let output = {
            let mut output = args.output;
            let mut file_name = output
                .file_name()
                .ok_or_else(|| anyhow!("no file name in path"))?
                .to_os_string();

            file_name.push(format!("_{:x}", args.config.genesis.rollup_type_hash));
            file_name.push(format!("_{}_{}", from_block, to_block));

            output.set_file_name(file_name);
            output
        };

        let export_block = ExportBlock {
            snap,
            output,
            from_block,
            to_block,
            progress_bar,
        };

        Ok(export_block)
    }

    // Disable warning for bin
    #[allow(dead_code)]
    pub fn store(&self) -> &StoreReadonly {
        &self.snap
    }

    pub fn execute(self) -> Result<()> {
        if let Some(parent) = self.output.parent() {
            fs::create_dir_all(parent)?;
        }
        self.write_to_mol()
    }

    pub fn write_to_mol(self) -> Result<()> {
        let f = fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(self.output)?;

        let mut writer = io::BufWriter::new(f);
        for block_number in self.from_block..=self.to_block {
            let exported_block = gw_utils::export_block::export_block(&self.snap, block_number)?;
            let packed: packed::ExportedBlock = exported_block.into();

            writer.write_all(packed.as_slice())?;

            if let Some(ref progress_bar) = self.progress_bar {
                progress_bar.inc(1)
            }
        }

        if let Some(ref progress_bar) = self.progress_bar {
            progress_bar.finish_with_message("done");
        }
        writer.flush()?;

        Ok(())
    }
}
