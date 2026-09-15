require_relative "user"

# A catalog of tagged users, block-heavy on purpose (do..end and brace
# forms both appear; the KP-6 block-shape facts are visible in this file).

class Catalog
  def initialize(users)
    @users = users
  end

  def by_tag(tag)
    @users.select { |user| user.tagged?(tag) }.each do |user|
      yield user if block_given?
    end
  end

  def banner(names)
    names.each_with_object(+"") do |name, acc|
      acc << format_line(name) << "; "
    end
  end

  private

  def format_line(name)
    "user: #{name}"
  end
end
