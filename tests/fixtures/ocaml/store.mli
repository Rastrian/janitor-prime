(* The store surface: the entry type plus the normalization funnel. *)

type entry =
  { key : string
  ; value : int
  }

val normalize_code : string -> string

(* Summaries render normalized keys. *)
val entry_key : entry -> string

val render : entry -> string

val total : entry list -> int
