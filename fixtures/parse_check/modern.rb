# frozen_string_literal: true

# Valid Ruby that an edit may write: DSLs, pattern matching and endless methods.
require "json"

class Post < ApplicationRecord
  has_many :comments, dependent: :destroy
  validates :title, presence: true, length: { maximum: 120 }
  scope :recent, -> { where(created_at: 1.week.ago..) }

  def self.build(**attrs) = new(**attrs)

  def summary = "#{title[0, 20]}…"

  def to_h(include: %i[title body], &block)
    data = include.to_h { |key| [key, public_send(key)] }
    block ? yield(data) : data
  end

  def classify(event)
    case event
    in { type: "comment", author: { name: String => name } }
      "comment by #{name}"
    in [Integer => id, *rest] if rest.empty?
      "id #{id}"
    in nil
      "none"
    else
      "other"
    end
  end
end

module Tools
  refine String do
    def shout = upcase + "!"
  end
end

x = 1
y = 2
point = { x:, y: }
words = %w[alpha beta].map { _1.capitalize }
text = <<~SQL
  SELECT * FROM posts WHERE id = #{point[:x]}
SQL

begin
  JSON.parse(text)
rescue JSON::ParserError => e
  warn e.message
ensure
  puts words&.first
end

task default: :spec
