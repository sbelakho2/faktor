package com.example.invoicing

import org.junit.jupiter.api.Test
import org.junit.jupiter.api.Assertions.assertEquals

class CartTotalsTest {
    @Test
    fun totalCentsAddUp() {
        val items = intArrayOf(10, 20)
        assertEquals(30, computeTotal(items))
        assertEquals(30, cartTotal(items))
    }
}
