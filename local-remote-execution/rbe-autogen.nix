{
  pkgs,
  buildImage,
  llvmPackages,
  ...
}: let
  customStdenv = import ../tools/llvmStdenv.nix {inherit pkgs llvmPackages;};

  # These dependencies are needed to generate the toolchain configurations but
  # aren't required during remote execution.
  autogenDeps = [
    # Required to generate toolchain configs.
    pkgs.bazel_7

    # Required for communication with trusted sources.
    pkgs.cacert

    # Tools that we would usually forward from the host.
    pkgs.bash
    pkgs.coreutils

    # We need these tools to generate the RBE autoconfiguration.
    pkgs.findutils
    pkgs.gnutar

    customStdenv.cc.bintools
  ];

  # A temporary directory. Note that this doesn't set any permissions. Those
  # need to be added explicitly in the final image arguments.
  mkTmp = pkgs.runCommand "mkTmp" {} ''
    mkdir -p $out/tmp
  '';

  # Permissions for the temporary directory.
  mkTmpPerms = {
    path = mkTmp;
    regex = ".*";
    mode = "1777";
    uid = 0; # Owned by root.
    gid = 0; # Owned by root.
  };

  # Enable the shebang `#!/usr/bin/env bash`.
  mkEnvSymlink = pkgs.runCommand "mkEnvSymlink" {} ''
    mkdir -p $out/usr/bin
    ln -s /bin/env $out/usr/bin/env
  '';

  user = "bazelbuild";
  group = "bazelbuild";
  uid = "1000";
  gid = "1000";

  mkUser = pkgs.runCommand "mkUser" {} ''
    mkdir -p $out/etc/pam.d

    echo "root:x:0:0::/root:${pkgs.runtimeShell}" > $out/etc/passwd
    echo "${user}:x:${uid}:${gid}:::" >> $out/etc/passwd

    echo "root:!x:::::::" > $out/etc/shadow
    echo "${user}:!x:::::::" >> $out/etc/shadow

    echo "root:x:0:" > $out/etc/group
    echo "${group}:x:${gid}:" >> $out/etc/group

    echo "root:x::" > $out/etc/gshadow
    echo "${group}:x::" >> $out/etc/gshadow

    cat > $out/etc/pam.d/other <<EOF
    account sufficient pam_unix.so
    auth sufficient pam_rootok.so
    password requisite pam_unix.so nullok sha512
    session required pam_unix.so
    EOF

    touch $out/etc/login.defs
    mkdir -p $out/home/${user}
  '';

  # Set permissions for the user's home directory.
  mkUserPerms = {
    path = mkUser;
    regex = "/home/${user}";
    mode = "0755";
    uid = pkgs.lib.toInt uid;
    gid = pkgs.lib.toInt gid;
    uname = user;
    gname = group;
  };
in
  image:
    buildImage {
      name = "autogen-${image.imageName}";
      fromImage = image;
      maxLayers = 20;
      copyToRoot = [
        mkUser
        mkTmp
        mkEnvSymlink
        (pkgs.buildEnv {
          name = "${image.imageName}-buildEnv";
          paths = autogenDeps;
          pathsToLink = ["/bin"];
        })
      ];

      perms = [
        mkUserPerms
        mkTmpPerms
      ];

      # Override the autogen container tag with the one from the toolchain
      # container. This way the autogen logic doesn't influence the toolchain
      # configuration.
      tag = image.imageTag;

      config = {
        User = user;
        WorkingDir = "/home/${user}";
        inherit (image.meta) Env;
      };
    }
