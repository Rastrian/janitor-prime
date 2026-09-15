# KP-6-elixir curated pattern 2: spec-above-def adjacency. `@spec` is a
# `unary_operator` wrapping a `call` (the grammar has no attribute kind);
# it parks as a named sibling directly above the def clause's `call`.
# Attribute siblings are content-bearing, so they are NOT trivia in
# either mode — adjacency holds in both.
defmodule Kp6.Spec do
  @spec add(integer(), integer()) :: integer()
  def add(a, b), do: a + b
end
