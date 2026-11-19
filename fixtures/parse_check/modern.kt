@file:JvmName("Modern")

package fixtures.parsecheck

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow

/** Valid Kotlin that an edit may write: sealed types, coroutines and builders. */
sealed interface Shape {
    data class Circle(val radius: Double) : Shape
    data object Empty : Shape
}

@JvmInline
value class UserId(val raw: String)

typealias Handler<T> = suspend (T) -> Unit

enum class Level(val weight: Int) {
    LOW(1),
    HIGH(10),
    ;

    fun heavier(other: Level) = weight > other.weight
}

inline fun <reified T : Any> describe(value: Any?): String = when (value) {
    is T -> "a ${T::class.simpleName}"
    null -> "nothing"
    in 1..10 -> "small"
    else -> "other: $value"
}

fun area(shape: Shape): Double = when (shape) {
    is Shape.Circle -> Math.PI * shape.radius * shape.radius
    Shape.Empty -> 0.0
}

fun numbers(limit: Int): Flow<Int> = flow {
    for (i in 0 until limit) emit(i)
}

class Repo(private val name: String) {
    val cache: Map<String, Int> by lazy { mapOf("a" to 1) }
    lateinit var handler: Handler<String>

    companion object {
        @JvmStatic
        fun create(name: String = "default") = Repo(name)
    }

    fun String.shout() = uppercase() + "!"

    suspend fun run(items: List<String>) {
        val (first, second) = items.take(2).let { it[0] to it.getOrElse(1) { "" } }
        val text = """
            |${first.shout()} $second
            """.trimMargin()
        items.forEachIndexed { index, item -> println("$index: ${item.length}") }
        handler(text)
        val comparator = object : Comparator<Int> {
            override fun compare(a: Int, b: Int) = a.compareTo(b)
        }
        println(listOf(3, 1).sortedWith(comparator))
    }
}
