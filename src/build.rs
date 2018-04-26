use std::fs::{self, File};
use std::{env, io, process};
use std::path::{Path, PathBuf};
use byteorder::{ByteOrder, LittleEndian};
use args::{self, Args};
use config::{self, Config};
use cargo_metadata::{self, Metadata as CargoMetadata, Package as CrateMetadata};
use Error;
use xmas_elf;
use tempdir::TempDir;

const BLOCK_SIZE: usize = 512;
type KernelInfoBlock = [u8; BLOCK_SIZE];

pub(crate) fn build(args: Args) -> Result<(), Error> {
    let (args, config, metadata, out_dir) = common_setup(args)?;

    build_impl(&args, &config, &metadata, &out_dir)
}

pub(crate) fn run(args: Args) -> Result<(), Error> {
    let (args, config, metadata, out_dir) = common_setup(args)?;

    build_impl(&args, &config, &metadata, &out_dir)?;
    run_impl(&args, &config)
}

fn common_setup(mut args: Args) -> Result<(Args, Config, CargoMetadata, PathBuf), Error> {
    fn out_dir(args: &Args, metadata: &CargoMetadata) -> PathBuf {
        let target_dir = PathBuf::from(&metadata.target_directory);
        let mut out_dir = target_dir;
        if let &Some(ref target) = args.target() {
            out_dir.push(target);
        }
        if args.release() {
            out_dir.push("release");
        } else {
            out_dir.push("debug");
        }
        out_dir
    }

    let metadata = read_cargo_metadata(&args)?;
    let crate_root = PathBuf::from(&metadata.workspace_root);
    let manifest_path = args.manifest_path().as_ref().map(Clone::clone).unwrap_or({
        let mut path = crate_root.clone();
        path.push("Cargo.toml");
        path
    });
    let config = config::read_config(manifest_path)?;

    if args.target().is_none() {
        if let Some(ref target) = config.default_target {
            args.set_target(target.clone());
        }
    }

    let out_dir = out_dir(&args, &metadata);

    Ok((args, config, metadata, out_dir))
}

fn build_impl(
    args: &Args,
    config: &Config,
    metadata: &CargoMetadata,
    out_dir: &Path,
) -> Result<(), Error> {
    let kernel = build_kernel(&out_dir, &args, &config, &metadata)?;

    let kernel_size = kernel.metadata()?.len();
    let kernel_info_block = create_kernel_info_block(kernel_size);

    if args.update_bootloader() {
        let mut bootloader_cargo_lock = PathBuf::from(out_dir);
        bootloader_cargo_lock.push("bootloader");
        bootloader_cargo_lock.push("Cargo.lock");

        fs::remove_file(bootloader_cargo_lock)?;
    }

    let tmp_dir = TempDir::new("bootloader")?;
    let bootloader = build_bootloader(tmp_dir.path(), &config)?;
    tmp_dir.close()?;

    create_disk_image(&config, kernel, kernel_info_block, &bootloader)?;

    Ok(())
}

fn run_impl(args: &Args, config: &Config) -> Result<(), Error> {
    let command = &config.run_command[0];
    let mut command = process::Command::new(command);
    for arg in &config.run_command[1..] {
        command.arg(
            arg.replace(
                "{}",
                config
                    .output
                    .to_str()
                    .expect("output must be valid unicode"),
            ),
        );
    }
    command.args(&args.run_args);
    command.status()?;
    Ok(())
}

fn read_cargo_metadata(args: &Args) -> Result<CargoMetadata, cargo_metadata::Error> {
    cargo_metadata::metadata(args.manifest_path().as_ref().map(PathBuf::as_path))
}

fn build_kernel(
    out_dir: &Path,
    args: &args::Args,
    config: &Config,
    metadata: &CargoMetadata,
) -> Result<File, Error> {
    let crate_ = metadata
        .packages
        .iter()
        .find(|p| Path::new(&p.manifest_path) == config.manifest_path)
        .expect("Could not read crate name from cargo metadata");
    let crate_name = &crate_.name;

    // compile kernel
    println!("Building kernel");
    let exit_status = run_xargo_build(&env::current_dir()?, &args.cargo_args)?;
    if !exit_status.success() {
        process::exit(1)
    }

    let mut kernel_path = out_dir.to_owned();
    kernel_path.push(crate_name);
    let kernel = File::open(kernel_path)?;
    Ok(kernel)
}

fn run_xargo_build(target_path: &Path, args: &[String]) -> io::Result<process::ExitStatus> {
    let mut command = process::Command::new("xargo");
    command.arg("build");
    command.env("RUST_TARGET_PATH", target_path);
    command.args(args);
    command.status()
}

fn create_kernel_info_block(kernel_size: u64) -> KernelInfoBlock {
    let kernel_size = if kernel_size <= u64::from(u32::max_value()) {
        kernel_size as u32
    } else {
        panic!("Kernel can't be loaded by BIOS bootloader because is too big")
    };

    let mut kernel_info_block = [0u8; BLOCK_SIZE];
    LittleEndian::write_u32(&mut kernel_info_block[0..4], kernel_size);

    kernel_info_block
}

fn download_bootloader(bootloader_dir: &Path, config: &Config) -> Result<CrateMetadata, Error> {
    use std::io::Write;

    let cargo_toml = {
        let mut dir = bootloader_dir.to_owned();
        dir.push("Cargo.toml");
        dir
    };
    let src_lib = {
        let mut dir = bootloader_dir.to_owned();
        dir.push("src");
        fs::create_dir_all(dir.as_path())?;
        dir.push("lib.rs");
        dir
    };

    {
        let mut cargo_toml_file = File::create(&cargo_toml)?;
        cargo_toml_file.write_all(
            r#"
            [package]
            authors = ["author@example.com>"]
            name = "bootloader_download_helper"
            version = "0.0.0"

        "#.as_bytes(),
        )?;
        cargo_toml_file.write_all(
            format!(
                r#"
            [dependencies.{}]
        "#,
                config.bootloader.name
            ).as_bytes(),
        )?;
        if let &Some(ref version) = &config.bootloader.version {
            cargo_toml_file.write_all(
                format!(
                    r#"
                    version = "{}"
            "#,
                    version
                ).as_bytes(),
            )?;
        }
        if let &Some(ref git) = &config.bootloader.git {
            cargo_toml_file.write_all(
                format!(
                    r#"
                    git = "{}"
            "#,
                    git
                ).as_bytes(),
            )?;
        }
        if let &Some(ref branch) = &config.bootloader.branch {
            cargo_toml_file.write_all(
                format!(
                    r#"
                    branch = "{}"
            "#,
                    branch
                ).as_bytes(),
            )?;
        }
        if let &Some(ref path) = &config.bootloader.path {
            cargo_toml_file.write_all(
                format!(
                    r#"
                    path = "{}"
            "#,
                    path.display()
                ).as_bytes(),
            )?;
        }

        File::create(src_lib)?.write_all(
            r#"
            #![no_std]
        "#.as_bytes(),
        )?;
    }

    let mut command = process::Command::new("cargo");
    command.arg("fetch");
    command.current_dir(bootloader_dir);
    assert!(command.status()?.success(), "Bootloader download failed.");

    let metadata = cargo_metadata::metadata_deps(Some(&cargo_toml), true)?;
    let bootloader = metadata
        .packages
        .iter()
        .find(|p| p.name == config.bootloader.name)
        .expect(&format!(
            "Could not find crate named “{}”",
            config.bootloader.name
        ));

    Ok(bootloader.clone())
}

fn build_bootloader(out_dir: &Path, config: &Config) -> Result<Box<[u8]>, Error> {
    use std::io::{Read, Write};

    let bootloader_metadata = download_bootloader(out_dir, config)?;
    let bootloader_dir = Path::new(&bootloader_metadata.manifest_path)
        .parent()
        .unwrap();

    let bootloader_elf_path = if !config.bootloader.precompiled {
        let args = &[
            String::from("--manifest-path"),
            bootloader_metadata.manifest_path.clone(),
            String::from("--target"),
            config.bootloader.target.clone(),
            String::from("--release"),
        ];

        println!("Building bootloader");
        let exit_status = run_xargo_build(bootloader_dir, args)?;
        if !exit_status.success() {
            process::exit(1)
        }

        let mut bootloader_elf_path = bootloader_dir.to_path_buf();
        bootloader_elf_path.push("target");
        bootloader_elf_path.push(&config.bootloader.target);
        bootloader_elf_path.push("release");
        bootloader_elf_path.push("bootloader");
        bootloader_elf_path
    } else {
        let mut bootloader_elf_path = bootloader_dir.to_path_buf();
        bootloader_elf_path.push("bootloader");
        bootloader_elf_path
    };

    let mut bootloader_elf_bytes = Vec::new();
    let mut bootloader = File::open(&bootloader_elf_path).map_err(|err| {
        Error::Bootloader(
            format!(
                "Could not open bootloader at {}",
                bootloader_elf_path.display()
            ),
            err,
        )
    })?;
    bootloader.read_to_end(&mut bootloader_elf_bytes)?;

    File::create(outdir(config).join("bootloader.elf"))?.write_all(&bootloader_elf_bytes)?;

    // copy bootloader section of ELF file to bootloader_path
    let elf_file = xmas_elf::ElfFile::new(&bootloader_elf_bytes).unwrap();
    xmas_elf::header::sanity_check(&elf_file).unwrap();
    let bootloader_section = elf_file
        .find_section_by_name(".bootloader")
        .expect("bootloader must have a .bootloader section");

    Ok(Vec::from(bootloader_section.raw_data(&elf_file)).into_boxed_slice())
}

#[inline]
fn outdir(config: &Config) -> PathBuf {
    let mut out = config.output.clone().canonicalize().expect("unable to get out directory");
    let _ = out.pop();
    out
}

fn create_disk_image(
    config: &Config,
    mut kernel: File,
    kernel_info_block: KernelInfoBlock,
    bootloader_data: &[u8],
) -> Result<(), Error> {
    use std::io::{Read, Write, Seek};

    println!("Creating disk image at {}", config.output.display());

    let _ = ::std::io::copy(&mut kernel, &mut File::create(outdir(config).join("kernel.elf"))?)?;
    let _ = kernel.seek(::std::io::SeekFrom::Start(0))?;

    let mut output = File::create(&config.output)?;
    output.write_all(&bootloader_data)?;
    output.write_all(&kernel_info_block)?;

    // write out kernel elf file
    let kernel_size = kernel.metadata()?.len();
    let mut buffer = [0u8; 1024];
    loop {
        let (n, interrupted) = match kernel.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => (n, false),
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => (0, true),
            Err(e) => Err(e)?,
        };
        if !interrupted {
            output.write_all(&buffer[..n])?
        }
    }

    let padding_size = ((512 - (kernel_size % 512)) % 512) as usize;
    let padding = [0u8; 512];
    output.write_all(&padding[..padding_size])?;

    if let Some(min_size) = config.minimum_image_size {
        // we already wrote to output successfully,
        // both metadata and set_len should succeed.
        if output.metadata()?.len() < min_size {
            output.set_len(min_size)?;
        }
    }

    Ok(())
}
