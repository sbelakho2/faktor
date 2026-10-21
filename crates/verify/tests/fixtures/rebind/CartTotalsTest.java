package com.example.invoicing;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.assertEquals;

class CartTotalsTest {
    private int[] items() {
        return new int[] { 10, 20 };
    }

    @Test
    void totalCentsAddUp() {
        int[] items = items();
        assertEquals(30, computeTotal(items));
        assertEquals(30, cartTotal(items));
    }
}
