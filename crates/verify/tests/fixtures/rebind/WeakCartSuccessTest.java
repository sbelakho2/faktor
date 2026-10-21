package com.example.invoicing;

import org.junit.Test;
import static org.junit.Assert.assertEquals;

class WeakCartTest {
    @Test
    void totalsAddUp() {
        int[] items = { 10, 20 };
        assertEquals(30, computeTotal(items));
        assertEquals(30, cartTotal(items));
    }
}
