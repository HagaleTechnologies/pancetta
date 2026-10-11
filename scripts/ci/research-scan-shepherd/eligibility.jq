# Scope over {pr, config}: decide.jq and shepherd.sh's unreadable-snapshot fallback
# share it, so neither acts on a PR outside the shepherd's scope.
def notes_path: type == "string" and startswith("research/notes/");
def ineligible_reasons:
  . as $s
  | [if ($s.pr.title | test($s.config.title_regex) | not) then "title" else empty end,
     if ($s.config.actors | index($s.pr.author)) == null then "author" else empty end,
     if $s.pr.isCrossRepository then "fork" else empty end,
     if $s.pr.isDraft then "draft" else empty end,
     if $s.pr.baseRefName != "main" then "base" else empty end,
     if ($s.pr.files | length) == 0 or
        any($s.pr.files[]; (.path | notes_path | not) or
          (.previous_path != null and (.previous_path | notes_path | not)))
     then "non-notes-files" else empty end];
