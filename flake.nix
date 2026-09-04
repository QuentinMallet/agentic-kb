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
        toolchain = fenix.packages.${system}.stable.withComponents [
          "cargo"
          "rustc"
          "rust-src"
          "clippy"
          "rustfmt"
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
            # Elixir MCP server (escript — no external hex deps, uses OTP 27 :json)
            mcpPackage = pkgs.stdenv.mkDerivation {
              pname = "agentic-kb-mcp";
              version = "0.1.0";
              src = ./mcp;
              nativeBuildInputs = [ pkgs.elixir_1_18 ];
              MIX_ENV = "prod";
              HEX_OFFLINE = "1";
              buildPhase = ''
                export HOME=$TMPDIR
                export MIX_HOME=$TMPDIR/.mix
                export HEX_HOME=$TMPDIR/.hex
                mix escript.build --no-deps-check
              '';
              installPhase = ''
                install -Dm755 agentic_kb_mcp $out/bin/agentic-kb-mcp
              '';
            };
          in
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
              doCheck = false;
              nativeBuildInputs = with pkgs; [ pkg-config cmake ];
              buildInputs = with pkgs; [ openssl ];
              OPENSSL_NO_VENDOR = "1";
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
            mcp = mcpPackage;
          }
          // pkgs.lib.optionalAttrs hasCargoLock {
            default = platform.buildRustPackage {
              pname = "kb";
              version = "0.1.0";
              src = ./.;
              cargoLock.lockFile = ./Cargo.lock;

              nativeBuildInputs = with pkgs; [ pkg-config cmake ];
              buildInputs = with pkgs; [ openssl ];

              OPENSSL_NO_VENDOR = "1";
              doCheck = false;

              meta = with pkgs.lib; {
                description = "Agent knowledge base CLI (SQLite + JSONL + semantic search)";
                mainProgram = "kb";
                platforms = platforms.unix;
              };
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
              pkg-config
              cmake
              openssl.dev
              tlaps
              tlaplus18
              mdbook
              hyperfine
              cargo-nextest
              act # Run GitHub Actions locally

              # Elixir MCP server (mcp/) — mix.exs requires OTP 27's :json module,
              # a hard floor. Pin the versioned, OTP-scoped attribute (not bare
              # `elixir`, which tracks the default BEAM set and can drift under it).
              beam27Packages.elixir

              # Local dev: secrets vault (OpenBao) + OIDC provider (Dex)
              openbao
              dex
            ]);

            shellHook = ''
                            export CARGO_HOME="$PWD/.cargo"
                            export PATH="$CARGO_HOME/bin:$PATH"
                            export OPENSSL_NO_VENDOR="1"
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
