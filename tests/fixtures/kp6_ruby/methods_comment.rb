# KP-6-ruby curated pattern 1: comment-between-methods adjacency.
# body_statement children: method, method, comment, method — the idiom
# once clean (hello/bye) and once comment-separated (bye/farewell).

class Greeting
  def hello
    "hi"
  end

  def bye
    "bye"
  end

  # a parting word, documented late
  def farewell
    "farewell"
  end
end
