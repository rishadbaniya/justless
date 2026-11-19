plugins {
    kotlin("jvm") version "2.0.0"
    id("application")
}

repositories {
    mavenCentral()
}

dependencies {
    implementation("org.jetbrains.kotlinx:kotlinx-coroutines-core:1.8.1")
    testImplementation(kotlin("test"))
}

application {
    mainClass.set("fixtures.parsecheck.ModernKt")
}

tasks.test {
    useJUnitPlatform()
}
