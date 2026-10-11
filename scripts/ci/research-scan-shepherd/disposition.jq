# The routine's first-line reply grammar. Keep its examples in the lifecycle doc tested.
def disposition_kind:
  split("\n")[0]
  | if test("^Fixed in [0-9a-f]{7,40}\\b") then "fixed"
    elif test("^Deferred to PAN-[0-9]+\\b") then "deferred"
    elif test("^Deferral blocked\\b") then "blocked"
    else null end;
def fixed_prefix: split("\n")[0] | capture("^Fixed in (?<sha>[0-9a-f]{7,40})").sha;
