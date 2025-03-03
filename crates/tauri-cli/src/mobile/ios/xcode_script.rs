// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use super::{ensure_init, env, get_app, get_config, read_options, MobileTarget};
use crate::{
  helpers::config::get as get_tauri_config,
  interface::{AppInterface, AppSettings, Interface, Options as InterfaceOptions},
  mobile::ios::LIB_OUTPUT_FILE_NAME,
  Result,
};

use anyhow::Context;
use cargo_mobile2::{apple::target::Target, opts::Profile};
use clap::Parser;

use std::{
  collections::HashMap,
  env::{current_dir, set_current_dir, var, var_os},
  ffi::OsStr,
  fs::read_to_string,
  path::{Path, PathBuf},
  process::Command,
};

#[derive(Debug, Parser)]
pub struct Options {
  /// Value of `PLATFORM_DISPLAY_NAME` env var
  #[clap(long)]
  platform: String,
  /// Value of `SDKROOT` env var
  #[clap(long)]
  sdk_root: PathBuf,
  /// Value of `FRAMEWORK_SEARCH_PATHS` env var
  #[clap(long)]
  framework_search_paths: String,
  /// Value of `GCC_PREPROCESSOR_DEFINITIONS` env var
  #[clap(long)]
  gcc_preprocessor_definitions: String,
  /// Value of `HEADER_SEARCH_PATHS` env var
  #[clap(long)]
  header_search_paths: String,
  /// Value of `CONFIGURATION` env var
  #[clap(long)]
  configuration: String,
  /// Value of `FORCE_COLOR` env var
  #[clap(long)]
  force_color: bool,
  /// Value of `ARCHS` env var
  #[clap(index = 1, required = true)]
  arches: Vec<String>,
}

pub fn command(options: Options) -> Result<()> {
  fn macos_from_platform(platform: &str) -> bool {
    platform == "macOS"
  }

  fn profile_from_configuration(configuration: &str) -> Profile {
    if configuration == "release" {
      Profile::Release
    } else {
      Profile::Debug
    }
  }

  // `xcode-script` is ran from the `gen/apple` folder when not using NPM.
  // so we must change working directory to the src-tauri folder to resolve the tauri dir
  if (var_os("npm_lifecycle_event").is_none() && var_os("PNPM_PACKAGE_NAME").is_none())
    || var("npm_config_user_agent").map_or(false, |agent| agent.starts_with("bun"))
  {
    set_current_dir(current_dir()?.parent().unwrap().parent().unwrap()).unwrap();
  }

  crate::helpers::app_paths::resolve();

  let profile = profile_from_configuration(&options.configuration);
  let macos = macos_from_platform(&options.platform);

  let tauri_config = get_tauri_config(tauri_utils::platform::Target::Ios, None)?;

  let (config, metadata, cli_options) = {
    let tauri_config_guard = tauri_config.lock().unwrap();
    let tauri_config_ = tauri_config_guard.as_ref().unwrap();
    let cli_options = read_options(&tauri_config_.identifier);
    let (config, metadata) = get_config(
      &get_app(
        MobileTarget::Ios,
        tauri_config_,
        &AppInterface::new(tauri_config_, None)?,
      ),
      tauri_config_,
      None,
      &cli_options,
    );
    (config, metadata, cli_options)
  };
  ensure_init(
    &tauri_config,
    config.app(),
    config.project_dir(),
    MobileTarget::Ios,
  )?;

  if let Some(config) = &cli_options.config {
    crate::helpers::config::merge_with(&config.0)?;
  }

  let env = env()?.explicit_env_vars(cli_options.vars);

  if !options.sdk_root.is_dir() {
    return Err(anyhow::anyhow!(
      "SDK root provided by Xcode was invalid. {} doesn't exist or isn't a directory",
      options.sdk_root.display(),
    ));
  }
  let include_dir = options.sdk_root.join("usr/include");
  if !include_dir.is_dir() {
    return Err(anyhow::anyhow!(
      "Include dir was invalid. {} doesn't exist or isn't a directory",
      include_dir.display()
    ));
  }

  // Host flags that are used by build scripts
  let macos_isysroot = {
    let macos_sdk_root = options
      .sdk_root
      .join("../../../../MacOSX.platform/Developer/SDKs/MacOSX.sdk");
    if !macos_sdk_root.is_dir() {
      return Err(anyhow::anyhow!(
        "Invalid SDK root {}",
        macos_sdk_root.display()
      ));
    }
    format!("-isysroot {}", macos_sdk_root.display())
  };

  let mut host_env = HashMap::<&str, &OsStr>::new();

  host_env.insert("RUST_BACKTRACE", "1".as_ref());

  host_env.insert("CFLAGS_x86_64_apple_darwin", macos_isysroot.as_ref());
  host_env.insert("CXXFLAGS_x86_64_apple_darwin", macos_isysroot.as_ref());

  host_env.insert(
    "OBJC_INCLUDE_PATH_x86_64_apple_darwin",
    include_dir.as_os_str(),
  );

  host_env.insert(
    "FRAMEWORK_SEARCH_PATHS",
    options.framework_search_paths.as_ref(),
  );
  host_env.insert(
    "GCC_PREPROCESSOR_DEFINITIONS",
    options.gcc_preprocessor_definitions.as_ref(),
  );
  host_env.insert("HEADER_SEARCH_PATHS", options.header_search_paths.as_ref());

  let macos_target = Target::macos();

  let isysroot = format!("-isysroot {}", options.sdk_root.display());

  for arch in options.arches {
    // Set target-specific flags
    let (env_triple, rust_triple) = match arch.as_str() {
      "arm64" => ("aarch64_apple_ios", "aarch64-apple-ios"),
      "arm64-sim" => ("aarch64_apple_ios_sim", "aarch64-apple-ios-sim"),
      "x86_64" => ("x86_64_apple_ios", "x86_64-apple-ios"),
      "Simulator" => {
        // when using Xcode, the arches for a simulator build will be ['Simulator', 'arm64-sim'] instead of ['arm64-sim']
        // so we ignore that on our end
        continue;
      }
      _ => {
        return Err(anyhow::anyhow!(
          "Arch specified by Xcode was invalid. {} isn't a known arch",
          arch
        ))
      }
    };

    let interface = AppInterface::new(
      tauri_config.lock().unwrap().as_ref().unwrap(),
      Some(rust_triple.into()),
    )?;

    let cflags = format!("CFLAGS_{}", env_triple);
    let cxxflags = format!("CFLAGS_{}", env_triple);
    let objc_include_path = format!("OBJC_INCLUDE_PATH_{}", env_triple);
    let mut target_env = host_env.clone();
    target_env.insert(cflags.as_ref(), isysroot.as_ref());
    target_env.insert(cxxflags.as_ref(), isysroot.as_ref());
    target_env.insert(objc_include_path.as_ref(), include_dir.as_ref());

    let target = if macos {
      &macos_target
    } else {
      Target::for_arch(&arch).ok_or_else(|| {
        anyhow::anyhow!(
          "Arch specified by Xcode was invalid. {} isn't a known arch",
          arch
        )
      })?
    };
    target.compile_lib(
      &config,
      &metadata,
      cli_options.noise_level,
      true,
      profile,
      &env,
      target_env,
    )?;

    let bin_path = interface
      .app_settings()
      .app_binary_path(&InterfaceOptions {
        debug: matches!(profile, Profile::Debug),
        target: Some(rust_triple.into()),
        ..Default::default()
      })?;
    let out_dir = bin_path.parent().unwrap();

    let lib_path = out_dir.join(format!("lib{}.a", config.app().lib_name()));
    if !lib_path.exists() {
      return Err(anyhow::anyhow!("Library not found at {}. Make sure your Cargo.toml file has a [lib] block with `crate-type = [\"staticlib\", \"cdylib\", \"lib\"]`", lib_path.display()));
    }

    // for some reason the app works on release, but `nm <path>` does not print the start_app symbol
    if profile == Profile::Debug {
      validate_lib(&lib_path)?;
    }

    let project_dir = config.project_dir();
    let externals_lib_dir = project_dir.join(format!("Externals/{arch}/{}", profile.as_str()));
    std::fs::create_dir_all(&externals_lib_dir)?;

    // backwards compatible lib output file name
    let uses_new_lib_output_file_name = {
      let pbxproj_contents = read_to_string(
        project_dir
          .join(format!("{}.xcodeproj", config.app().name()))
          .join("project.pbxproj"),
      )
      .context("missing project.pbxproj file in the Xcode project")?;

      pbxproj_contents.contains(LIB_OUTPUT_FILE_NAME)
    };

    let lib_output_file_name = if uses_new_lib_output_file_name {
      LIB_OUTPUT_FILE_NAME.to_string()
    } else {
      format!("lib{}.a", config.app().lib_name())
    };

    std::fs::copy(lib_path, externals_lib_dir.join(lib_output_file_name))?;
  }
  Ok(())
}

fn validate_lib(path: &Path) -> Result<()> {
  // we ignore `nm` errors
  if let Ok(output) = Command::new("nm").arg(path).output() {
    let symbols = String::from_utf8_lossy(&output.stdout);
    if !symbols.contains("start_app") {
      anyhow::bail!(
      "Library from {} does not include required runtime symbols. This means you are likely missing the tauri::mobile_entry_point macro usage, see the documentation for more information: https://v2.tauri.app/start/migrate/from-tauri-1",
      path.display()
    );
    }
  }
  Ok(())
}
