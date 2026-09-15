(* Bundled ocaml fixture corpus (E5 disposition): the fixed body of
   real-shape code the LangRegistry seeds its kind table from at first
   use. Parsed only; never executed. *)

let square x = x * x

let cube x =
  let s = square x in
  s * x

(* A documented unit exercising modules, types and functions. *)
module Widget = struct
  type kind = Alpha | Beta | Gamma

  type t =
    { id : int
    ; kind : kind
    }

  let make id kind = { id; kind }

  let classify t =
    if t.id mod 2 = 0 && t.id > 0 then Some "even"
    else if t.id < 0 then None
    else Some "odd"

  let tally rows =
    List.fold_left (fun acc (key, value) -> acc + value * (key + 1)) 0 rows

  let () = print_endline "widgetry loaded"
end

let comprehended items =
  let squares =
    List.filter_map (fun n -> if n = 0 then None else Some (n * n)) items
  in
  let total = List.fold_left ( + ) 0 squares in
  (squares, total)

let banner () =
  Format.printf "widgetry: %d items\n" (List.length (comprehended [ 1; 2; 3 ]))
