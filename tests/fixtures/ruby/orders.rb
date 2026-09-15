require_relative "user"

# Order summary lines render through the same normalization funnel.

class Orders
  def initialize(users)
    @users = users
  end

  def summary(user)
    code = normalize_code(user.email)
    "#{user.id}: #{code}"
  end

  # Labels normalize each tag independently.
  def labels(user)
    user.tags.map { |tag| normalize_code(tag) }
  end
end
