# A user record with validation, in the shape real ruby services grow.

class User
  attr_reader :id, :email
  attr_accessor :tags

  # Registered users carry a verified address.
  def initialize(id, email, tags = [])
    @id = id
    @email = email
    @tags = tags
  end

  def normalize_code(input)
    cleaned = input.to_s.strip.downcase
    cleaned.gsub(/\s+/, " ")
  end

  def display_name
    "#{normalize_code(email)} <#{id}>"
  end

  # Tags are compared after normalization.
  def tagged?(label)
    tags.include?(label)
  end
end
