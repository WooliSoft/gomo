{
  description = "Gomo, monorepo tooling for Gleam packages";

  nixConfig = {
    extra-substituters = ["https://gomo.cachix.org"];
    extra-trusted-public-keys = [
      "gomo.cachix.org-1:caU+N40akKkMLsv/G1IiUekhIwh7d6IlKcLRGiJqzv0="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  };

  outputs = {
    self,
    nixpkgs,
  }: let
    systems = [
      "x86_64-linux"
      "aarch64-linux"
      "aarch64-darwin"
    ];

    forAllSystems = function:
      nixpkgs.lib.genAttrs systems (system:
        function (import nixpkgs {inherit system;}));

    cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
  in {
    packages = forAllSystems (pkgs: let
      inherit (pkgs) lib;

      source = lib.cleanSourceWith {
        src = ./.;
        filter = path: type: let
          root = toString ./.;
          rel = lib.removePrefix "${root}/" (toString path);
        in
          !(rel == "target" || lib.hasPrefix "target/" rel || rel == "result");
      };

      darwinFrameworks = lib.optionals pkgs.stdenv.isDarwin (with pkgs.darwin.apple_sdk.frameworks; [
        Security
        SystemConfiguration
      ]);

      gomo = pkgs.rustPlatform.buildRustPackage {
        pname = cargoToml.package.name;
        version = cargoToml.package.version;

        src = source;
        cargoLock.lockFile = ./Cargo.lock;

        nativeBuildInputs = [
          pkgs.installShellFiles
          pkgs.pkg-config
        ];

        nativeCheckInputs = [
          pkgs.jujutsu
        ];

        buildInputs = [
          pkgs.zlib
          pkgs.zstd
        ] ++ darwinFrameworks;

        postInstall = ''
          installShellCompletion --cmd gomo \
            --bash <($out/bin/gomo completions bash) \
            --fish <($out/bin/gomo completions fish) \
            --zsh <($out/bin/gomo completions zsh)
        '';

        meta = {
          description = cargoToml.package.description;
          homepage = "https://github.com/WooliSoft/gomo";
          license = lib.licenses.mit;
          mainProgram = "gomo";
          platforms = systems;
        };
      };
    in {
      inherit gomo;
      default = gomo;
    });

    apps = forAllSystems (pkgs: let
      gomo = self.packages.${pkgs.stdenv.hostPlatform.system}.gomo;
    in {
      gomo = {
        type = "app";
        program = "${gomo}/bin/gomo";
        meta.description = "Monorepo tooling for Gleam packages";
      };

      default = self.apps.${pkgs.stdenv.hostPlatform.system}.gomo;
    });
  };
}
