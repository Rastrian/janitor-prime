# KP-6-elixir curated pattern 1: comment-between-function-headers.
# Elixir functions are successive `def` clauses — `def add(a, b), do:` and
# `def add(a, b, c) do` are BOTH `call` nodes (no dedicated def kind), named
# siblings inside the module's `do_block`; the comment between successive
# clauses of the SAME name is what byte-strict loses.
defmodule Kp6.Heads do
  def add(a, b), do: a + b

  # between successive clauses of the same head
  def add(a, b, c) do
    a + b + c
  end
end
