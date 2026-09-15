# KP-6-ruby curated pattern 3: block (do..end) as sibling vs child.
# Measured fact: both block forms attach as the `block` field CHILD of
# their call — do..end is a `do_block` node wrapping a body_statement,
# braces are a `block` node wrapping a block_body. Neither is ever a
# statement-level sibling, in either trivia mode.

def sweep(items)
  items.each do |item|
    puts item
  end
  items.map { |item| item.to_s }
end
