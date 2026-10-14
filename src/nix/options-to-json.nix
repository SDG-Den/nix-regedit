# Emits one level of a flake's option registry as JSON.
#
# The option set is a nested attrset: intermediate nodes are plain attrsets
# (namespaces, shown as folders), leaves are options (`_type == "option"`).
# Each call returns only the children of `path`, which keeps evaluation shallow
# and memory bounded (a full recursive tree forces the whole module system).
#
# Arguments:
#   flakePath : absolute path (string) to the flake root.
#   target    : "" to list available configurations, otherwise one of
#               "fp"                 flake-parts debug options
#               "nixos:<host>"       a NixOS configuration
#               "home:<user>"        a standalone home-manager configuration
#               "hm:<host>:<user>"   home-manager nested in a NixOS host
#               "darwin:<host>"      a nix-darwin configuration
#   path      : attribute path whose children to return (list of strings).
#   detail    : when true, return value/declarations/definitions of the single
#               option at `path` instead of the child list.
{flakePath, target ? "", path ? [], detail ? false}: let
  root = builtins.getFlake flakePath;

  safe = default: e: let
    r = builtins.tryEval e;
  in
    if r.success
    then r.value
    else default;

  split = s: builtins.filter builtins.isString (builtins.split ":" s);

  # Shallow, non-forcing value preview. Never descends into lists or attrsets,
  # so it cannot trigger the NixOS module system's evaluation assertions.
  pretty = v:
    if builtins.isBool v
    then (if v then "true" else "false")
    else if builtins.isInt v || builtins.isFloat v
    then builtins.toString v
    else if builtins.isString v
    then (
      if builtins.stringLength v > 200
      then builtins.substring 0 200 v + "…"
      else v
    )
    else if v == null
    then "null"
    else if builtins.isList v
    then "[ ${builtins.toString (builtins.length v)} items ]"
    else if builtins.isAttrs v
    then (
      if v ? outPath
      then (v.name or v.outPath)
      else "{ ${builtins.toString (builtins.length (builtins.attrNames v))} attrs }"
    )
    else "<value>";

  isOption = x: builtins.isAttrs x && (x._type or null) == "option";

  # Options may set `visible = "shallow"`; coerce anything non-bool to default.
  asBool = default: x: let
    v = safe default x;
  in
    if builtins.isBool v
    then v
    else default;

  mkPos = p: {
    file = p.file or "";
    line = p.line or 0;
    column = p.column or 0;
  };

  safePositions = xs:
    builtins.concatLists (builtins.map (x:
      let
        r = builtins.tryEval (
          if builtins.isAttrs x
          then mkPos x
          else throw "not a position"
        );
      in
        if r.success
        then [r.value]
        else [])
    xs);

  # Definition files can carry a ", via option <path>" suffix; drop it.
  cleanFile = f: builtins.head (builtins.split ", via " f);

  # Newer nixpkgs exposes positions in `declarationPositions`; older ones put
  # attributed positions in `declarations`. Prefer the former.
  declsOf = opt:
    let
      ps = safe [] (opt.declarationPositions or []);
      fallback = safe [] (opt.declarations or []);
    in
      if ps != []
      then safePositions ps
      else safePositions fallback;

  # `definitionsWithLocations` carries only {file, value} on current nixpkgs
  # (no line/column); the editor locates the assignment by attr path instead.
  defsOf = opt:
    builtins.concatLists (builtins.map (d:
      let
        r = builtins.tryEval {
          file = cleanFile (d.file or "");
          line = 0;
          column = 0;
          value =
            if d ? value
            then safe null (pretty d.value)
            else null;
        };
      in
        if r.success
        then [r.value]
        else [])
    (safe [] (opt.definitionsWithLocations or [])));

  lookup = loc:
    builtins.foldl'
    (acc: k:
      if builtins.isAttrs acc && acc ? ${k}
      then acc.${k}
      else null)
    config
    loc;

  keepName = n: n != "_module";

  isNode = x: isOption x || builtins.isAttrs x;

  # Children of the option or namespace `name` (at absolute path `seg`) inside
  # `parent`.
  childrenOf = seg: name: parent: let
    child = parent.${name};
  in
    if isOption child
    then safe {} (child.type.getSubOptions seg)
    else if builtins.isAttrs child
    then child
    else {};

  # Descend to the option set at `path` (namespaces and option submodules).
  childrenAt = acc: rest: opts:
    if rest == []
    then opts
    else let
      n = builtins.head rest;
      seg = acc ++ [n];
    in
      childrenAt seg (builtins.tail rest) (childrenOf seg n opts);

  # The option or namespace attrset at `path`, or null.
  childAt = acc: rest: opts:
    if rest == []
    then null
    else let
      n = builtins.head rest;
      seg = acc ++ [n];
      child = opts.${n};
    in
      if builtins.tail rest == []
      then child
      else childAt seg (builtins.tail rest) (childrenOf seg n opts);

  # One level only: children at `loc`. No recursion.
  walk = loc: opts: let
    names = builtins.filter (n: keepName n && isNode opts.${n}) (builtins.attrNames opts);
  in
    builtins.map (n: let
      childLoc = loc ++ [n];
      child = opts.${n};
      opt = isOption child;
      sub = childrenOf childLoc n opts;
    in {
      name = n;
      path = builtins.concatStringsSep "." childLoc;
      segments = childLoc;
      type =
        if opt
        then safe "unspecified" (child.type.description or "unspecified")
        else "namespace";
      description =
        if opt
        then safe "" (child.description or "")
        else "";
      read_only = opt && asBool false (child.readOnly or false);
      visible = opt && asBool true (child.visible or true);
      internal = opt && asBool false (child.internal or false);
      # "Set" = the flake actively assigns this option (non-empty definitions).
      # Wrapped in tryEval: some freeform/submodule defaults throw when forced,
      # and a failure is treated as set so nothing is silently hidden.
      set =
        if opt
        then let
          r = builtins.tryEval (builtins.length (child.definitions or []));
        in
          if r.success
          then r.value > 0
          else true
        else false;
      has_children = builtins.length (builtins.attrNames sub) > 0;
      # value and definitions are loaded on demand (see the `detail` branch):
      # forcing them evaluates the whole system configuration.
    })
    names;

  nixosNames =
    if root ? nixosConfigurations
    then builtins.attrNames root.nixosConfigurations
    else [];

  homeNames =
    if root ? homeConfigurations
    then builtins.attrNames root.homeConfigurations
    else [];

  darwinNames =
    if root ? darwinConfigurations
    then builtins.attrNames root.darwinConfigurations
    else [];

  parts = split target;
  kind = builtins.head (parts ++ [""]);
  p1 = builtins.elemAt (parts ++ ["" ""]) 1;
  p2 = builtins.elemAt (parts ++ ["" "" ""]) 2;

  resolved =
    if target == ""
    then {}
    else if kind == "fp"
    then {
      options = root.debug.options or null;
      config = safe null (root.debug.config or null);
    }
    else if kind == "nixos"
    then {
      options = root.nixosConfigurations.${p1}.options or null;
      config = safe null (root.nixosConfigurations.${p1}.config or null);
    }
    else if kind == "home"
    then {
      options = root.homeConfigurations.${p1}.options or null;
      config = safe null (root.homeConfigurations.${p1}.config or null);
    }
    else if kind == "hm"
    then {
      options = safe null (root.nixosConfigurations.${p1}.options.home-manager.users.type.getSubOptions []);
      config = safe null (root.nixosConfigurations.${p1}.config.home-manager.users.${p2} or null);
    }
    else if kind == "darwin"
    then {
      options = root.darwinConfigurations.${p1}.options or null;
      config = safe null (root.darwinConfigurations.${p1}.config or null);
    }
    else if kind == "hmusers"
    then {
      options = null;
      config = null;
    }
    else {};

  options = resolved.options or null;
  config = resolved.config or null;

  hmUsers =
    if kind == "hmusers"
    then
      safe [] (
        builtins.attrNames (
          safe {} (root.nixosConfigurations.${p1}.config.home-manager.users)
        )
      )
    else [];
in
  if target == ""
  then {
    configs = {
      nixos = nixosNames;
      home = homeNames;
      darwin = darwinNames;
      flake_parts = root ? debug;
    };
  }
  else if kind == "hmusers"
  then {users = hmUsers;}
  else if detail
  then let
    opt = safe null (childAt [] path options);
    isOpt = opt != null && isOption opt;
    sub = safe {} (
      if isOpt
      then opt.type.getSubOptions path
      else if builtins.isAttrs opt
      then opt
      else {}
    );
  in {
    config_name = target;
    path = builtins.concatStringsSep "." path;
    value =
      if !isOpt || builtins.length (builtins.attrNames sub) > 0
      then null
      else safe null (pretty (lookup path));
    declarations =
      if isOpt
      then declsOf opt
      else [];
    definitions =
      if isOpt
      then defsOf opt
      else [];
  }
  else {
    config_name = target;
    path = builtins.concatStringsSep "." path;
    options =
      if options == null
      then []
      else walk path (childrenAt [] path options);
  }
