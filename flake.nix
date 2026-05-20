{
  inputs = {
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
    nixpkgs.url = "nixpkgs/nixos-unstable";
  };

  outputs =
    {
      fenix,
      flake-utils,
      nixpkgs,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:

      let
        target = "aarch64-unknown-linux-musl";
        toolchain =
          with fenix.packages.${system};
          combine [
            stable.cargo
            stable.rustc
            targets.${target}.stable.rust-std
          ];
        pkgs = nixpkgs.legacyPackages.${system};
        platform = pkgs.makeRustPlatform {
          cargo = toolchain;
          rustc = toolchain;
        };
      in
      {
        packages =
          let
            omcSrc =
              if builtins.pathExists ./.omc then
                builtins.path {
                  path = ./.omc;
                  name = "omc-planning-artifacts";
                  filter =
                    path: _type:
                    let
                      rel = pkgs.lib.removePrefix (toString ./.omc + "/") path;
                    in
                    pkgs.lib.hasPrefix "plans/" rel
                    || pkgs.lib.hasPrefix "specs/" rel
                    || pkgs.lib.hasPrefix "research/" rel
                    || rel == "";
                }
              else
                null;
            mkGuideDocs =
              {
                includePlanning ? false,
              }:
              pkgs.stdenv.mkDerivation {
                name = "package-guide";
                src = ./docs;
                nativeBuildInputs = [ pkgs.mdbook ];
                buildPhase = ''
                  cp -r $src build-docs
                  chmod -R u+w build-docs
                  cd build-docs
                  ${pkgs.lib.optionalString (includePlanning && omcSrc != null) ''
                    chmod +x scripts/generate-planning-artifacts.sh
                    bash scripts/generate-planning-artifacts.sh ${omcSrc} src
                  ''}
                  mdbook build --dest-dir $out
                '';
                dontInstall = true;
              };
            hasCargoLock = builtins.pathExists ./Cargo.lock;
            apiDocs = platform.buildRustPackage {
              name = "package-rustdoc";
              dontCheck = true;
              nativeBuildInputs = with pkgs; [ cmake ];
              cargoLock.lockFile = ./Cargo.lock;
              src = ./.;
              buildPhase = "cargo doc --offline --no-deps";
              installPhase = ''
                mkdir -p $out
                cp -a target/doc/. $out/
              '';
            };
            mkDoc =
              {
                includePlanning ? false,
              }:
              if hasCargoLock then
                pkgs.runCommand "package-doc" { } ''
                  mkdir -p $out/guide $out/api
                  cp -r ${mkGuideDocs { inherit includePlanning; }}/. $out/guide/
                  cp -r ${apiDocs}/. $out/api/
                ''
              else
                mkGuideDocs { inherit includePlanning; };
          in
          {
            doc = mkDoc { };
            doc-with-planning = mkDoc { includePlanning = true; };
          }
          // pkgs.lib.optionalAttrs hasCargoLock {
            default = platform.buildRustPackage {
              pname = "package";
              nativeBuildInputs = with pkgs; [ cmake ];
              buildInputs = with pkgs; [ stdenv.cc.cc.lib ];
              version = "0.1.0";

              src = ./.;

              cargoLock.lockFile = ./Cargo.lock;
            };
          };
        devShells = {
          default = pkgs.mkShell {
            buildInputs = [
              (fenix.packages.${system}.stable.withComponents [
                "cargo"
                "clippy"
                "rust-src"
                "rustc"
                "rustfmt"
              ])
            ]
            ++ (with pkgs; [
              cmake
              tlaps
              tlaplus18
              mdbook
              act # Run GitHub Actions locally

              # Local dev: secrets vault (OpenBao) + OIDC provider (Dex)
              openbao
              dex
            ]);

            shellHook = ''
                            export CARGO_HOME="$PWD/.cargo"
                            export PATH="$CARGO_HOME/bin:$PATH"
                            export LD_LIBRARY_PATH="${pkgs.stdenv.cc.cc.lib}/lib";
                            mkdir -p .cargo
                            echo '*' > .cargo/.gitignore

                            # Local dev: secrets vault (OpenBao) + OIDC provider (Dex)
                            export BAO_ADDR="''${BAO_ADDR:-http://127.0.0.1:8200}"
                            if [ ! -f .dev/dex.yaml ]; then
                              mkdir -p .dev
                              cat > .dev/dex.yaml.tmp <<'DEX_EOF'
              issuer: http://127.0.0.1:5556/dex
              storage:
                type: memory
              web:
                http: 127.0.0.1:5556
              staticClients:
                - id: dev-client
                  redirectURIs:
                    - http://127.0.0.1:8080/callback
                  name: Dev Client
                  secret: dev-secret
              enablePasswordDB: true
              staticPasswords:
                - email: admin@example.com
                  hash: "$2a$10$2b2cU8CPhOTaGrs1HRQuAueS7JTT5ZHsHSzYiFPm1leZck7Mc8T4W"
                  username: admin
                  userID: 08a8684b-db88-4b73-90a9-3cd1661f5466
              DEX_EOF
                              mv .dev/dex.yaml.tmp .dev/dex.yaml
                            fi
                            echo "Dev secrets: bao server -dev  (OpenBao on :8200)"
                            echo "Dev auth:    dex serve .dev/dex.yaml  (Dex OIDC on :5556)"
            '';
          };
        };
      }
    );
}
