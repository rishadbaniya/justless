package fixtures.parsecheck

import scala.concurrent.{ExecutionContext, Future}

/** Valid Scala that an edit may write: case classes, implicits, givens and enums. */
sealed trait Shape
final case class Circle(radius: Double) extends Shape
case object Empty extends Shape

enum Color(val rgb: Int):
  case Red extends Color(0xff0000)
  case Green extends Color(0x00ff00)

trait Show[A]:
  def show(a: A): String

given Show[Int] with
  def show(a: Int): String = s"int $a"

extension (s: String)
  def shout: String = s.toUpperCase + "!"

object Modern:
  def area(shape: Shape): Double = shape match
    case Circle(r) if r > 0 => math.Pi * r * r
    case _                  => 0.0

  def render[A](a: A)(using show: Show[A]): String = show.show(a)

  def fetch(ids: List[Int])(implicit ec: ExecutionContext): Future[List[String]] =
    Future.traverse(ids)(id => Future(f"$id%04d"))

  def pairs(n: Int): Seq[(Int, Int)] =
    for
      i <- 1 to n
      j <- i to n
      if (i + j) % 2 == 0
    yield (i, j)

  def legacy(xs: List[Int]): Int = {
    xs.foldLeft(0) { (acc, x) =>
      if (x > 0) acc + x else acc
    }
  }

  @main def run(): Unit =
    val label = if pairs(3).nonEmpty then "some" else "none"
    println(s"${label.shout} ${render(1)}")
