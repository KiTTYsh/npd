{ lib
, rustPlatform
, cmake
, perl
, pkg-config
, aws-lc
, sqlite
, mpv-unwrapped
}:

rustPlatform.buildRustPackage {
  pname = "npd";
  version = (lib.importTOML ../Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../contrib
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    # aws-lc-sys (via reqwest's rustls stack) needs these if it ever falls back
    # to building its vendored C sources instead of linking system aws-lc.
    cmake
    perl
  ];

  # cmake is only a dependency of a build script, not of this crate.
  dontUseCmakeConfigure = true;

  # mpv-unwrapped is libmpv itself (libmpv2-sys links -lmpv). nixpkgs' rust
  # hooks tell aws-lc-sys and libsqlite3-sys to link system libraries rather
  # than build their vendored copies, so both have to be here too.
  buildInputs = [ mpv-unwrapped aws-lc sqlite ];

  postInstall = ''
    install -Dm644 -t $out/share/doc/npd \
      contrib/config.example.toml contrib/npd.service
  '';

  meta = {
    description = "MPD-compatible daemon backed by Navidrome / OpenSubsonic";
    mainProgram = "npd";
    platforms = lib.platforms.linux;
  };
}
