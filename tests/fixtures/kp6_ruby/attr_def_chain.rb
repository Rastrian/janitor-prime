# KP-6-ruby curated pattern 2: attr_*/def sibling chains. attr_reader and
# attr_accessor are plain `call` nodes (there is no dedicated attr kind),
# named siblings of `method` inside body_statement; the comment between
# the attr chain and the first def is what byte-strict loses.

class Settings
  attr_reader :name
  attr_accessor :value

  # every setting renders through one funnel
  def render
    "#{name}=#{value}"
  end

  def reset!
    self.value = nil
  end
end
